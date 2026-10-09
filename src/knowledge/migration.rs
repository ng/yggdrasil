//! Explicit single-host private migration orchestration. Operator declarations
//! cover offline clients/editors that PostgreSQL cannot enumerate. Shared/fleet
//! execution is deliberately rejected until its corresponding protocol exists.
use super::{
    clients,
    cutover::PrivateJournal,
    document::digest,
    export,
    guard::CLIENT_PROTOCOL,
    identity::{Identities, IdentityRegistry},
    legacy::Mappings,
    rollback::RecoveryPaths,
    runtime::{Binding, Phase, SELECTION_FILE},
    source_backup::SourceBackup,
    store::KnowledgeStore,
};
use crate::{config::database::DeploymentConfig, db};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use std::{
    collections::BTreeMap,
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub name: String,
    pub protocol: i32,
    pub knowledge_writers_stopped: bool,
    pub external_editors_stopped: bool,
    pub schema_changes_stopped: bool,
    pub session_preserving_endpoint: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub transport: String,
    pub source_generation: i64,
    pub mappings: Mappings,
    pub identities: Identities,
    pub agents: BTreeMap<String, Uuid>,
    pub execution_host: String,
    pub all_participating_hosts_listed: bool,
    pub hosts: Vec<Host>,
}
impl Plan {
    fn validate(&self) -> Result<()> {
        self.identities.validate()?;
        for (legacy, portable) in &self.mappings.repos {
            ensure!(
                self.identities.repos.iter().any(|r| r.id == *portable
                    && r.databases
                        .get(&self.mappings.database_id)
                        .is_some_and(|ids| ids.contains(legacy))),
                "identity policy lacks an explicit legacy repository mapping"
            );
        }
        ensure!(
            self.version == 1
                && self.source_generation > 0
                && self.source_generation < i64::MAX - 2,
            "unsupported migration plan version/generation"
        );
        ensure!(
            self.transport == "private" && self.hosts.len() == 1,
            "execution currently requires a private corpus on one declared host; shared/fleet execution is not available"
        );
        let host = &self.hosts[0];
        ensure!(
            self.all_participating_hosts_listed
                && !host.name.trim().is_empty()
                && host.name.len() <= 128
                && host.name == self.execution_host
                && host.protocol == CLIENT_PROTOCOL
                && host.knowledge_writers_stopped
                && host.external_editors_stopped
                && host.schema_changes_stopped
                && host.session_preserving_endpoint,
            "explicit complete host census, compatible protocol, writer/editor/schema quiescence and session-preserving endpoint declarations required"
        );
        ensure!(
            !self.mappings.database_id.is_nil()
                && !self.mappings.corpus_id.is_nil()
                && self.identities.version == 1
                && self.identities.corpus_id == self.mappings.corpus_id,
            "plan identity differs from explicit mapping"
        );
        ensure!(
            self.agents
                .iter()
                .all(|(name, id)| !name.trim().is_empty() && !id.is_nil()),
            "invalid explicit agent mapping"
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
    corpus: PathBuf,
    policy: PathBuf,
    plan: Plan,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyIntent {
    identity: String,
    fenced: String,
    root_identity: (u64, u64),
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub operation: Uuid,
    pub database_id: Uuid,
    pub corpus_id: Uuid,
    pub generation: i64,
    pub state: String,
    pub journal: PathBuf,
}

pub struct Journal {
    store: KnowledgeStore,
    intent: Intent,
    bytes: String,
    _lease: File,
}
fn target(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "absolute migration paths required");
    Ok(path
        .parent()
        .context("migration path parent required")?
        .canonicalize()?
        .join(path.file_name().context("migration path name required")?))
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
        let corpus = target(&config.knowledge_dir)?;
        let policy = target(&config.knowledge_policy_dir)?;
        for (a, b) in [
            (&directory, &corpus),
            (&directory, &policy),
            (&corpus, &policy),
        ] {
            ensure!(
                !a.starts_with(b) && !b.starts_with(a),
                "journal, corpus and policy must be separate"
            );
        }
        let store = KnowledgeStore::open(&directory, true)?;
        let lease = store.operation_lock(".export.lock")?;
        store.verify_root_path(&directory)?;
        if let Some(bytes) = store.read_artifact("migration-intent.json")? {
            let intent: Intent = serde_json::from_str(&bytes)?;
            ensure!(
                intent.version == 1
                    && !intent.operation.is_nil()
                    && intent.directory == directory
                    && intent.corpus == corpus
                    && intent.policy == policy
                    && serde_json::to_value(&intent.plan)? == serde_json::to_value(&plan)?,
                "migration journal differs from plan, paths or original directory"
            );
            return Ok(Self {
                store,
                intent,
                bytes,
                _lease: lease,
            });
        }
        if exists(&policy)? {
            let current = KnowledgeStore::open(&policy, false)?;
            ensure!(
                current.read_control(SELECTION_FILE)?.is_none(),
                "new forward migration requires unselected SQL knowledge; use the existing journal to resume"
            );
            ensure!(
                current.read_control("shared.json")?.is_none(),
                "private migration cannot replace shared configuration"
            );
        }
        let intent = Intent {
            version: 1,
            operation: Uuid::new_v4(),
            directory,
            corpus,
            policy,
            plan,
        };
        let bytes = serde_json::to_string(&intent)?;
        store.retain_artifact("migration-intent.json", &bytes, true)?;
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
            self.store
                .read_artifact("migration-intent.json")?
                .as_deref()
                == Some(&self.bytes),
            "migration intent changed"
        );
        ensure!(
            target(&config.knowledge_dir)? == self.intent.corpus
                && target(&config.knowledge_policy_dir)? == self.intent.policy,
            "configured knowledge paths changed during migration"
        );
        Ok(())
    }
    fn child(&self, name: &str) -> PathBuf {
        self.intent.directory.join(name)
    }
    fn request_hash(&self, backup: &SourceBackup) -> String {
        digest(format!("{}\n{}", self.bytes, backup.digest()).as_bytes())
    }
    async fn backup(
        &self,
        config: &DeploymentConfig,
        bin: Option<&Path>,
        create: bool,
    ) -> Result<SourceBackup> {
        let path = self.child("source-backup");
        if !exists(&path)? {
            ensure!(
                create,
                "source backup missing; cannot abort an unbacked operation"
            );
            db::deployment_backup::create(config, &path, bin, None).await?;
        }
        let saved = SourceBackup::open(
            &path,
            self.intent.plan.mappings.database_id,
            self.intent.plan.source_generation,
        )?;
        saved.verify_configuration(config)?;
        self.store.retain_artifact(
            "source-backup-digest.json",
            &serde_json::to_string(saved.digest())?,
            false,
        )?;
        Ok(saved)
    }
    async fn transaction<'a>(&self, pool: &PgPool) -> Result<Transaction<'a, Postgres>> {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        let owner:bool=sqlx::query_scalar("SELECT pg_catalog.pg_has_role(session_user,relowner,'USAGE') AND pg_catalog.pg_has_role(current_user,relowner,'USAGE') FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass").fetch_one(&mut *tx).await?;
        ensure!(owner, "knowledge migration requires migration owner");
        sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
    async fn marker(
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<(Uuid, i64, i32, String, Option<Uuid>)> {
        Ok(sqlx::query_as("SELECT database_id,generation,minimum_client,backend,corpus_id FROM public.knowledge_storage WHERE singleton FOR UPDATE").fetch_one(&mut **tx).await?)
    }
    async fn event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        step: &str,
        backup: &SourceBackup,
    ) -> Result<bool> {
        let prior:Option<(String,Uuid,i64,Option<Uuid>)>=sqlx::query_as("SELECT request_sha256,database_id,target_generation,corpus_id FROM public.knowledge_migration_events WHERE operation_id=$1 AND step=$2")
            .bind(self.intent.operation).bind(step).fetch_optional(&mut **tx).await?;
        if let Some(prior) = prior {
            let fenced = step == "fenced";
            ensure!(
                prior
                    == (
                        self.request_hash(backup),
                        self.intent.plan.mappings.database_id,
                        self.intent.plan.source_generation + if fenced { 1 } else { 2 },
                        fenced.then_some(self.intent.plan.mappings.corpus_id)
                    ),
                "maintenance event conflicts with saved operation"
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }
    async fn record(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        step: &str,
        backup: &SourceBackup,
    ) -> Result<()> {
        let fenced = step == "fenced";
        sqlx::query("INSERT INTO public.knowledge_migration_events(operation_id,step,request_sha256,database_id,target_generation,corpus_id) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(self.intent.operation).bind(step).bind(self.request_hash(backup)).bind(self.intent.plan.mappings.database_id)
            .bind(self.intent.plan.source_generation+if fenced {1}else{2}).bind(fenced.then_some(self.intent.plan.mappings.corpus_id))
            .execute(&mut **tx).await?;
        Ok(())
    }
    async fn fence(&self, pool: &PgPool, backup: &SourceBackup) -> Result<()> {
        let p = &self.intent.plan;
        let mut tx = self.transaction(pool).await?;
        let marker = Self::marker(&mut tx).await?;
        ensure!(
            marker.0 == p.mappings.database_id && marker.2 > 0 && marker.2 <= CLIENT_PROTOCOL,
            "incompatible source database/protocol"
        );
        ensure!(
            !self.event(&mut tx, "aborted", backup).await?,
            "migration was aborted; prepare a new journal for the current SQL generation"
        );
        if self.event(&mut tx, "fenced", backup).await? {
            ensure!(
                marker.1 == p.source_generation + 1
                    && marker.3 == "fenced"
                    && marker.4 == Some(p.mappings.corpus_id),
                "recorded fence is no longer current; use existing activation journal or inspect later transitions"
            );
        } else {
            ensure!(
                marker.1 == p.source_generation && marker.3 == "sql" && marker.4.is_none(),
                "source is not the planned SQL generation"
            );
            ensure!(
                clients::audit(&mut tx).await?.live_blockers == 0,
                "unregistered or outdated live clients block fencing"
            );
            backup.verify_on(&mut tx).await?;
            sqlx::query("UPDATE public.knowledge_storage SET backend='fenced',generation=$1,corpus_id=$2 WHERE singleton")
                .bind(p.source_generation+1).bind(p.mappings.corpus_id).execute(&mut *tx).await?;
            self.record(&mut tx, "fenced", backup).await?;
        }
        backup.verify_on(&mut tx).await?;
        tx.commit()
            .await
            .context("fence commit outcome uncertain; resume the same migration journal")
    }
    fn prepare_policy(&self, manifest: &export::Manifest) -> Result<()> {
        let policy = KnowledgeStore::open(&self.intent.policy, true)?;
        let _selection = policy.selection_lease(true)?;
        policy.verify_root_path(&self.intent.policy)?;
        let metadata = std::fs::symlink_metadata(&self.intent.policy)?;
        let identity = serde_json::to_string_pretty(&self.intent.plan.identities)?;
        let binding = Binding {
            version: 1,
            minimum_client: CLIENT_PROTOCOL,
            generation: manifest.generation,
            phase: Phase::Fenced,
            bundle: self.intent.corpus.clone(),
            mappings: serde_json::from_value(manifest.mappings.clone())?,
            agents: self.intent.plan.agents.clone(),
        };
        let desired = PolicyIntent {
            identity,
            fenced: serde_json::to_string(&binding)?,
            root_identity: (metadata.dev(), metadata.ino()),
        };
        let bytes = serde_json::to_string(&desired)?;
        self.store
            .retain_artifact("prepared-policy.json", &bytes, false)?;
        // Existing policy must already agree semantically. Preserve its bytes;
        // never replace independent trust, aliases or approval-lead decisions.
        policy.update_control("identity.json", |current| {
            if let Some(current) = current {
                ensure!(
                    serde_json::from_str::<serde_json::Value>(current)?
                        == serde_json::from_str::<serde_json::Value>(&desired.identity)?,
                    "existing identity/trust policy differs from migration plan"
                );
                return Ok((current.to_owned(), ()));
            }
            Ok((desired.identity.clone(), ()))
        })?;
        let registry = IdentityRegistry::open(&self.intent.policy, false)?;
        registry.read()?;
        for (legacy, portable) in &binding.mappings.repos {
            ensure!(
                registry.from_legacy(binding.mappings.database_id, *legacy)? == *portable,
                "prepared repository mapping differs"
            );
        }
        policy.update_control(SELECTION_FILE, |current| {
            ensure!(
                current.is_none() || current == Some(desired.fenced.as_str()),
                "local selection conflicts with prepared migration"
            );
            Ok((desired.fenced.clone(), ()))
        })?;
        Ok(())
    }
    fn activation_prepared(&self) -> Result<bool> {
        let path = self.child("activation");
        if !exists(&path)? {
            return Ok(false);
        }
        Ok(KnowledgeStore::open(&path, false)?
            .read_artifact("forward-intent.json")?
            .is_some())
    }
    pub async fn execute(&self, config: &DeploymentConfig, bin: Option<&Path>) -> Result<Report> {
        self.check(config)?;
        let pool = db::maintenance_pool(config).await?;
        let result = self.execute_with_pool(config, bin, &pool).await;
        pool.close().await;
        result
    }
    async fn execute_with_pool(
        &self,
        config: &DeploymentConfig,
        bin: Option<&Path>,
        pool: &PgPool,
    ) -> Result<Report> {
        // Fail before creating a dump if the compatibility migration is missing.
        let ready: bool = sqlx::query_scalar(
            "SELECT to_regclass('public.knowledge_migration_events') IS NOT NULL",
        )
        .fetch_one(pool)
        .await?;
        ensure!(
            ready,
            "run explicit database migrations before preparing knowledge cutover"
        );
        if !exists(&self.child("source-backup"))? {
            let mut tx = self.transaction(pool).await?;
            let marker = Self::marker(&mut tx).await?;
            let p = &self.intent.plan;
            ensure!(
                marker.0 == p.mappings.database_id
                    && marker.1 == p.source_generation
                    && marker.2 > 0
                    && marker.2 <= CLIENT_PROTOCOL
                    && marker.3 == "sql"
                    && marker.4.is_none(),
                "source is not the planned compatible SQL generation"
            );
            super::source_backup::verify_guards(&mut tx).await?;
            ensure!(
                clients::audit(&mut tx).await?.live_blockers == 0,
                "unregistered or outdated live clients block preparation"
            );
            tx.rollback().await?;
            ensure!(
                super::inventory::assess(pool, Some(&p.mappings))
                    .await?
                    .rows_verified,
                "resolve inventory mappings and conversion failures before execution"
            );
        }
        let backup = self.backup(config, bin, true).await?;
        if !self.activation_prepared()? {
            self.fence(pool, &backup).await?;
            self.check(config)?;
            let manifest =
                export::stage(pool, &self.intent.plan.mappings, &self.child("stage")).await?;
            export::publish(
                pool,
                &self.child("stage"),
                &self.child("publication-backup"),
                &self.intent.corpus,
            )
            .await?;
            self.prepare_policy(&manifest)?;
            let corpus = KnowledgeStore::open(&self.intent.corpus, false)?;
            let policy = KnowledgeStore::open(&self.intent.policy, false)?;
            let _selection = policy.selection_lease(true)?;
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
            PrivateJournal::prepare(&self.child("activation"), manifest, &recovery, paths)?;
        }
        self.check(config)?;
        PrivateJournal::open(&self.child("activation"))?
            .activate_verified(pool, &backup)
            .await?;
        self.check(config)?;
        Ok(self.report("okf", self.intent.plan.source_generation + 2))
    }
    fn report(&self, state: &str, generation: i64) -> Report {
        Report {
            operation: self.intent.operation,
            database_id: self.intent.plan.mappings.database_id,
            corpus_id: self.intent.plan.mappings.corpus_id,
            generation,
            state: state.into(),
            journal: self.intent.directory.clone(),
        }
    }
    /// Pre-activation abort only. Once OKF was selected, reverse import of current
    /// documents is mandatory; this operation can never revive stale SQL rows.
    pub async fn abort(&self, config: &DeploymentConfig) -> Result<Report> {
        self.check(config)?;
        let backup = self.backup(config, None, false).await?;
        let policy = if exists(&self.intent.policy)? {
            Some(KnowledgeStore::open(&self.intent.policy, false)?)
        } else {
            None
        };
        let _selection = policy
            .as_ref()
            .map(|p| p.selection_lease(true))
            .transpose()?;
        let prepared = self
            .store
            .read_artifact("prepared-policy.json")?
            .map(|s| serde_json::from_str::<PolicyIntent>(&s))
            .transpose()?;
        if let Some(policy) = &policy {
            if let Some(current) = policy.read_control(SELECTION_FILE)? {
                ensure!(
                    prepared.as_ref().is_some_and(|p| p.fenced == current),
                    "local selection is active or independently changed; abort refused"
                );
            }
            if let Some(prepared) = &prepared {
                let meta = std::fs::symlink_metadata(&self.intent.policy)?;
                ensure!(
                    (meta.dev(), meta.ino()) == prepared.root_identity,
                    "policy directory replaced; abort refused"
                );
            }
        }
        let pool = db::maintenance_pool(config).await?;
        let result=async {
            let mut tx=self.transaction(&pool).await?;
            let marker=Self::marker(&mut tx).await?;
            let p=&self.intent.plan;
            if self.event(&mut tx,"aborted",&backup).await? {
                ensure!(marker.0==p.mappings.database_id && marker.1==p.source_generation+2 && marker.3=="sql" && marker.4.is_none(),"recorded abort is no longer current");
            } else {
                ensure!(self.event(&mut tx,"fenced",&backup).await?,"this operation does not own a committed fence");
                ensure!(marker.0==p.mappings.database_id && marker.1==p.source_generation+1 && marker.3=="fenced" && marker.4==Some(p.mappings.corpus_id),"abort only supports the original fence; active OKF requires current-bundle reverse import");
                backup.verify_on(&mut tx).await?;
                sqlx::query("UPDATE public.knowledge_storage SET backend='sql',generation=$1,corpus_id=NULL WHERE singleton")
                    .bind(p.source_generation+2).execute(&mut *tx).await?;
                self.record(&mut tx,"aborted",&backup).await?;
            }
            self.check(config)?;
            tx.commit().await.context("abort outcome uncertain; resume abort with the same journal")?;
            // Commit can succeed before the local unlink. Reacquire a lease and
            // recheck the selected generation so another transition cannot race it.
            let mut selected = pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED").execute(&mut *selected).await?;
            sqlx::query("SELECT pg_advisory_xact_lock_shared(1497843531,1)").execute(&mut *selected).await?;
            let current: (Uuid,i64,String,Option<Uuid>) = sqlx::query_as("SELECT database_id,generation,backend,corpus_id FROM public.knowledge_storage WHERE singleton").fetch_one(&mut *selected).await?;
            ensure!(current == (p.mappings.database_id,p.source_generation+2,"sql".into(),None), "abort generation changed before local deselection");
            self.check(config)?;
            if let (Some(policy),Some(prepared))=(&policy,&prepared) {
                policy.verify_root_path(&self.intent.policy)?;
                policy.remove_control(SELECTION_FILE,&prepared.fenced)?;
            }
            selected.commit().await?;
            Ok(self.report("aborted",p.source_generation+2))
        }.await;
        pool.close().await;
        result
    }
}
