//! Private single-host rollback of the current OKF corpus. Declarations cover
//! offline participants/editors; shared/fleet recovery needs its own coordinator.
use super::{
    clients,
    document::digest,
    export, fence,
    guard::CLIENT_PROTOCOL,
    migration::{Host, Report},
    recovery_event::{Event, owner_transaction},
    reverse,
    rollback::{self, RecoveryPaths},
    runtime::{Binding, Phase, SELECTION_FILE},
    source_backup::SourceBackup,
    store::KnowledgeStore,
};
use crate::{
    config::database::{DeploymentConfig, KnowledgeConfig},
    db,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub transport: String,
    pub source_generation: i64,
    /// Retained original staging export, not the edited authoritative corpus.
    pub original_export: PathBuf,
    pub execution_host: String,
    pub all_participating_hosts_listed: bool,
    pub hosts: Vec<Host>,
}
impl Plan {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.source_generation > 0
                && self.source_generation < i64::MAX - 2,
            "unsupported rollback plan version/generation"
        );
        ensure!(
            self.transport == "private" && self.hosts.len() == 1,
            "rollback currently requires a private corpus on one declared host"
        );
        let h = &self.hosts[0];
        ensure!(
            self.all_participating_hosts_listed
                && !h.name.trim().is_empty()
                && h.name.len() <= 128
                && h.name == self.execution_host
                && h.protocol == CLIENT_PROTOCOL
                && h.knowledge_writers_stopped
                && h.external_editors_stopped
                && h.schema_changes_stopped
                && h.session_preserving_endpoint,
            "explicit complete host census, compatible protocol, writer/editor/schema quiescence and session-preserving endpoint declarations required"
        );
        ensure!(
            self.original_export.is_absolute(),
            "absolute original export path required"
        );
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32,
    operation: Uuid,
    directory: PathBuf,
    plan: Plan,
    corpus: PathBuf,
    policy: PathBuf,
    corpus_identity: (u64, u64),
    policy_identity: (u64, u64),
    original: export::Manifest,
    selected: String,
    fenced: String,
}
pub struct Journal {
    store: KnowledgeStore,
    intent: Intent,
    bytes: String,
    _lease: File,
}
fn identity(path: &Path) -> Result<(u64, u64)> {
    let m = std::fs::symlink_metadata(path)?;
    ensure!(m.is_dir(), "recovery root replaced");
    Ok((m.dev(), m.ino()))
}
fn target(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "absolute recovery paths required");
    Ok(path
        .parent()
        .context("recovery parent required")?
        .canonicalize()?
        .join(path.file_name().context("recovery name required")?))
}
fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
impl Journal {
    pub fn prepare(directory: &Path, plan: Plan, config: &DeploymentConfig) -> Result<Self> {
        plan.validate()?;
        let directory = target(directory)?;
        let corpus = config.knowledge_dir.canonicalize()?;
        let policy = config.knowledge_policy_dir.canonicalize()?;
        let original_path = plan.original_export.canonicalize()?;
        for (a, b) in [
            (&directory, &corpus),
            (&directory, &policy),
            (&directory, &original_path),
            (&corpus, &policy),
            (&corpus, &original_path),
            (&policy, &original_path),
        ] {
            ensure!(
                !a.starts_with(b) && !b.starts_with(a),
                "recovery journal, source roots and original export must be separate"
            );
        }
        let original = export::verify(&original_path)?;
        ensure!(
            plan.source_generation > original.generation,
            "rollback requires a later selected OKF generation"
        );
        let store = KnowledgeStore::open(&directory, true)?;
        let lease = store.operation_lock(".export.lock")?;
        store.verify_root_path(&directory)?;
        if let Some(bytes) = store.read_artifact("recovery-intent.json")? {
            let intent: Intent = serde_json::from_str(&bytes)?;
            ensure!(
                intent.version == 1
                    && !intent.operation.is_nil()
                    && intent.directory == directory
                    && intent.corpus == corpus
                    && intent.policy == policy
                    && intent.original == original
                    && serde_json::to_value(&intent.plan)? == serde_json::to_value(&plan)?,
                "recovery journal differs from plan, paths or original export"
            );
            let saved = Self {
                store,
                intent,
                bytes,
                _lease: lease,
            };
            saved.check(config)?;
            return Ok(saved);
        }
        let policy_store = KnowledgeStore::open(&policy, false)?;
        let _selection = policy_store.selection_lease(true)?;
        ensure!(
            policy_store.read_control("shared.json")?.is_none(),
            "private rollback cannot handle shared configuration"
        );
        let selected = policy_store
            .read_control(SELECTION_FILE)?
            .context("selected OKF corpus required")?;
        let mut binding: Binding = serde_json::from_str(&selected)?;
        ensure!(
            binding.version == 1
                && binding.minimum_client > 0
                && binding.minimum_client <= CLIENT_PROTOCOL
                && binding.generation == plan.source_generation
                && binding.phase == Phase::Okf
                && binding.bundle == corpus
                && serde_json::to_value(&binding.mappings)? == original.mappings,
            "selected corpus differs from rollback plan/export"
        );
        binding.phase = Phase::Fenced;
        let intent = Intent {
            version: 1,
            operation: Uuid::new_v4(),
            directory,
            plan,
            corpus_identity: identity(&corpus)?,
            policy_identity: identity(&policy)?,
            corpus,
            policy,
            original,
            selected,
            fenced: serde_json::to_string(&binding)?,
        };
        let bytes = serde_json::to_string(&intent)?;
        store.retain_artifact("recovery-intent.json", &bytes, true)?;
        Ok(Self {
            store,
            intent,
            bytes,
            _lease: lease,
        })
    }
    fn check(&self, config: &DeploymentConfig) -> Result<()> {
        self.intent.plan.validate()?;
        self.store.verify_root_path(&self.intent.directory)?;
        ensure!(
            self.store.read_artifact("recovery-intent.json")?.as_deref()
                == Some(self.bytes.as_str()),
            "recovery intent changed"
        );
        ensure!(
            config.knowledge_dir.canonicalize()? == self.intent.corpus
                && config.knowledge_policy_dir.canonicalize()? == self.intent.policy
                && identity(&self.intent.corpus)? == self.intent.corpus_identity
                && identity(&self.intent.policy)? == self.intent.policy_identity,
            "recovery source root changed"
        );
        ensure!(
            export::verify(&self.intent.plan.original_export)? == self.intent.original,
            "original export changed"
        );
        Ok(())
    }
    fn child(&self, name: &str) -> PathBuf {
        self.intent.directory.join(name)
    }
    async fn backup(
        &self,
        config: &DeploymentConfig,
        bin: Option<&Path>,
        pool: &PgPool,
    ) -> Result<SourceBackup> {
        let p = &self.intent.plan;
        let original = &self.intent.original;
        if !exists(&self.child("source-backup"))? {
            let mut tx = owner_transaction(pool).await?;
            let marker:(Uuid,i64,i32,String,Option<Uuid>)=sqlx::query_as("SELECT database_id,generation,minimum_client,backend,corpus_id FROM public.knowledge_storage WHERE singleton FOR UPDATE").fetch_one(&mut *tx).await?;
            ensure!(
                marker.0 == original.database_id
                    && marker.1 == p.source_generation
                    && marker.2 > 0
                    && marker.2 <= CLIENT_PROTOCOL
                    && marker.3 == "okf"
                    && marker.4 == Some(original.corpus_id),
                "rollback source is not planned OKF generation"
            );
            super::source_backup::verify_guards(&mut tx).await?;
            ensure!(
                clients::audit(&mut tx).await?.live_blockers == 0,
                "incompatible live clients block rollback preparation"
            );
            tx.rollback().await?;
            db::deployment_backup::create(config, &self.child("source-backup"), bin, None).await?;
        }
        let backup = SourceBackup::open_okf(
            &self.child("source-backup"),
            original.database_id,
            p.source_generation,
            original.corpus_id,
        )?;
        backup.verify_configuration(config)?;
        self.store.retain_artifact(
            "source-backup-digest.json",
            &serde_json::to_string(backup.digest())?,
            false,
        )?;
        Ok(backup)
    }
    fn fence_event(&self, backup: &SourceBackup) -> Event {
        Event {
            operation: self.intent.operation,
            step: "fenced",
            request: digest(format!("{}\n{}", self.bytes, backup.digest()).as_bytes()),
            database: self.intent.original.database_id,
            corpus: self.intent.original.corpus_id,
            generation: self.intent.plan.source_generation + 1,
        }
    }
    /// Validate representability under source leases before publishing any local
    /// fence. The SQL fence exists only inside a rolled-back preflight transaction.
    async fn preflight_current(&self, pool: &PgPool, backup: &SourceBackup) -> Result<()> {
        let policy = KnowledgeStore::open(&self.intent.policy, false)?;
        let _selection = policy.selection_lease(true)?;
        let current = policy.read_control(SELECTION_FILE)?;
        if current.as_deref() == Some(self.intent.fenced.as_str()) {
            return Ok(());
        }
        ensure!(
            current.as_deref() == Some(self.intent.selected.as_str()),
            "rollback selection changed before preflight"
        );
        let corpus = KnowledgeStore::open(&self.intent.corpus, false)?;
        let recovery = corpus.resume_pair_retained(
            &policy,
            &self.child("source-backup/knowledge"),
            &self.child("source-backup/policy"),
        )?;
        let mut tx = owner_transaction(pool).await?;
        let marker:(Uuid,i64,String,Option<Uuid>)=sqlx::query_as("SELECT database_id,generation,backend,corpus_id FROM public.knowledge_storage WHERE singleton FOR UPDATE").fetch_one(&mut *tx).await?;
        ensure!(
            marker
                == (
                    self.intent.original.database_id,
                    self.intent.plan.source_generation,
                    "okf".into(),
                    Some(self.intent.original.corpus_id)
                ),
            "rollback preflight requires planned OKF generation"
        );
        backup.verify_on(&mut tx).await?;
        sqlx::query("UPDATE public.knowledge_storage SET backend='fenced',generation=generation+1 WHERE singleton").execute(&mut *tx).await?;
        let candidate = reverse::capture_recovery_on(
            &mut tx,
            &self.intent.original,
            &recovery,
            self.intent.plan.source_generation + 1,
        )
        .await?;
        // Exercise typed SQL conversion and constraints too, then roll back all
        // row changes and the temporary marker. No reverse receipt is published.
        reverse::apply_on(&mut tx, &candidate, self.intent.plan.source_generation + 1).await?;
        tx.rollback().await?;
        Ok(())
    }
    async fn fence(
        &self,
        pool: &PgPool,
        backup: &SourceBackup,
        config: &DeploymentConfig,
    ) -> Result<()> {
        // Local fencing happens before the database transition so offline writers
        // cannot continue accepting OKF writes once recovery reaches SQL.
        let config = KnowledgeConfig {
            data_dir: config.data_dir.clone(),
            knowledge_dir: self.intent.corpus.clone(),
            knowledge_policy_dir: self.intent.policy.clone(),
        };
        fence::local(&config, self.intent.plan.source_generation)?;
        let policy = KnowledgeStore::open(&self.intent.policy, false)?;
        let _selection = policy.selection_lease(true)?;
        ensure!(
            policy.read_control(SELECTION_FILE)?.as_deref() == Some(self.intent.fenced.as_str()),
            "local recovery fence differs from original selection"
        );
        let mut tx = owner_transaction(pool).await?;
        let event = self.fence_event(backup);
        let marker:(Uuid,i64,i32,String,Option<Uuid>)=sqlx::query_as("SELECT database_id,generation,minimum_client,backend,corpus_id FROM public.knowledge_storage WHERE singleton FOR UPDATE").fetch_one(&mut *tx).await?;
        ensure!(
            marker.0 == event.database
                && marker.2 > 0
                && marker.2 <= CLIENT_PROTOCOL
                && marker.4 == Some(event.corpus),
            "incompatible rollback source identity/protocol"
        );
        if event.recorded(&mut tx).await? {
            ensure!(
                marker.1 == event.generation && marker.3 == "fenced",
                "recorded rollback fence is no longer current"
            );
        } else {
            ensure!(
                marker.1 == event.generation - 1 && marker.3 == "okf",
                "rollback operation does not own current fence"
            );
            ensure!(
                clients::audit(&mut tx).await?.live_blockers == 0,
                "incompatible live clients block recovery fencing"
            );
            backup.verify_on(&mut tx).await?;
            sqlx::query("UPDATE public.knowledge_storage SET backend='fenced',generation=$1 WHERE singleton").bind(event.generation).execute(&mut *tx).await?;
            event.record(&mut tx).await?;
        }
        backup.verify_on(&mut tx).await?;
        tx.commit()
            .await
            .context("recovery fence outcome uncertain; resume same journal")
    }
    pub async fn execute(&self, config: &DeploymentConfig, bin: Option<&Path>) -> Result<Report> {
        self.check(config)?;
        let pool = db::maintenance_pool(config).await?;
        let result = self.execute_on(config, bin, &pool).await;
        pool.close().await;
        result
    }
    async fn execute_on(
        &self,
        config: &DeploymentConfig,
        bin: Option<&Path>,
        pool: &PgPool,
    ) -> Result<Report> {
        super::inventory::require_utf8(&mut *pool.acquire().await?).await?;
        let ready: bool = sqlx::query_scalar(
            "SELECT to_regclass('public.knowledge_recovery_events') IS NOT NULL",
        )
        .fetch_one(pool)
        .await?;
        ensure!(ready, "run explicit database migrations before rollback");
        let backup = self.backup(config, bin, pool).await?;
        if !exists(&self.child("import/rollback-intent.json"))? {
            self.preflight_current(pool, &backup).await?;
            self.fence(pool, &backup, config).await?;
            self.check(config)?;
            let policy = KnowledgeStore::open(&self.intent.policy, false)?;
            let _selection = policy.selection_lease(true)?;
            ensure!(
                policy.read_control(SELECTION_FILE)?.as_deref()
                    == Some(self.intent.fenced.as_str()),
                "local recovery fence changed"
            );
            let corpus = KnowledgeStore::open(&self.intent.corpus, false)?;
            let paths = RecoveryPaths {
                corpus: self.intent.corpus.clone(),
                policy: self.intent.policy.clone(),
                corpus_archive: self.child("corpus-backup"),
                policy_archive: self.child("policy-backup"),
            };
            let recovery = corpus.complete_pair_retained(
                &policy,
                &paths.corpus_archive,
                &paths.policy_archive,
            )?;
            let mut tx = owner_transaction(pool).await?;
            ensure!(
                self.fence_event(&backup).recorded(&mut tx).await?,
                "rollback fence receipt missing"
            );
            backup.verify_on(&mut tx).await?;
            let candidate = reverse::capture_recovery_on(
                &mut tx,
                &self.intent.original,
                &recovery,
                self.intent.plan.source_generation + 1,
            )
            .await?;
            rollback::Journal::prepare(
                &self.child("import"),
                self.intent.original.clone(),
                candidate,
                &recovery,
                paths,
                self.intent.plan.source_generation + 1,
                None,
            )?;
            self.check(config)?;
            tx.rollback().await?;
        }
        self.check(config)?;
        rollback::Journal::open(&self.child("import"))?
            .complete_private(pool, &backup)
            .await?;
        self.check(config)?;
        Ok(Report {
            operation: self.intent.operation,
            database_id: self.intent.original.database_id,
            corpus_id: self.intent.original.corpus_id,
            generation: self.intent.plan.source_generation + 2,
            state: "sql".into(),
            journal: self.intent.directory.clone(),
        })
    }
}
