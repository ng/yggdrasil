//! Durable host-local completion of a private-corpus forward cutover. The
//! coordinator must first back up the source database, fence SQL, validate and
//! publish the export, prepare explicit identity/trust and a fenced local binding,
//! and quiesce every participating host/external editor. This journal records the
//! file evidence and operation ID before SQL activation; it is not a fleet census.
use super::{
    export::Manifest,
    forward::{self, Outcome},
    identity::IdentityRegistry,
    rollback::RecoveryPaths,
    runtime::{Binding, Phase, SELECTION_FILE},
    store::{KnowledgeBackup, KnowledgeStore, PairedBackup},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const INTENT: &str = "forward-intent.json";
fn identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir(), "cutover directory replaced");
    Ok((metadata.dev(), metadata.ino()))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    manifest: Manifest,
    paths: RecoveryPaths,
    corpus_identity: (u64, u64),
    policy_identity: (u64, u64),
    corpus_revision: String,
    policy_revision: String,
    fenced: String,
    selected: String,
}
impl Request {
    fn roots(&self) -> Result<()> {
        ensure!(
            identity(&self.paths.corpus)? == self.corpus_identity
                && identity(&self.paths.policy)? == self.policy_identity,
            "cutover source directory identity changed"
        );
        Ok(())
    }
    fn validate(&self, directory: &Path) -> Result<()> {
        ensure!(
            self.manifest.version == 1
                && self.manifest.generation > 0
                && !self.manifest.database_id.is_nil()
                && !self.manifest.corpus_id.is_nil(),
            "invalid cutover manifest identity/version"
        );
        let paths = [
            &self.paths.corpus,
            &self.paths.policy,
            &self.paths.corpus_archive,
            &self.paths.policy_archive,
        ];
        for (index, path) in paths.iter().enumerate() {
            ensure!(
                path.is_absolute() && path.canonicalize()? == **path,
                "canonical absolute cutover paths required"
            );
            ensure!(
                !path.starts_with(directory) && !directory.starts_with(path),
                "journal must be separate from sources and archives"
            );
            for other in &paths[index + 1..] {
                ensure!(
                    !path.starts_with(other) && !other.starts_with(path),
                    "cutover sources and archives must be separate"
                );
            }
        }
        let mut binding: Binding = serde_json::from_str(&self.fenced)?;
        ensure!(
            binding.version == 1
                && binding.minimum_client > 0
                && binding.minimum_client <= super::guard::CLIENT_PROTOCOL
                && binding.phase == Phase::Fenced
                && binding.generation == self.manifest.generation
                && binding.bundle == self.paths.corpus
                && serde_json::to_value(&binding.mappings)? == self.manifest.mappings
                && binding.mappings.database_id == self.manifest.database_id
                && binding.mappings.corpus_id == self.manifest.corpus_id,
            "cutover requires matching prepared fenced binding"
        );
        binding.phase = Phase::Okf;
        binding.generation = binding
            .generation
            .checked_add(1)
            .context("generation overflow")?;
        ensure!(
            serde_json::to_string(&binding)? == self.selected,
            "selected binding differs from saved cutover intent"
        );
        ensure!(
            KnowledgeBackup::verify(&self.paths.corpus_archive)?.revision == self.corpus_revision
                && KnowledgeBackup::verify(&self.paths.policy_archive)?.revision
                    == self.policy_revision,
            "cutover recovery archive changed"
        );
        self.roots()
    }
    fn frozen(&self, recovery: &PairedBackup) -> Result<()> {
        self.roots()?;
        recovery.verify_paths(&self.paths.corpus, &self.paths.policy)?;
        ensure!(
            recovery.corpus().revision == self.corpus_revision
                && recovery.policy().revision == self.policy_revision,
            "cutover source differs from saved recovery evidence"
        );
        let snapshot = recovery.snapshot()?; // Also rejects shared transport.
        ensure!(
            snapshot.documents.len() == self.manifest.entries.len(),
            "published corpus count differs from manifest"
        );
        let mut expected = std::collections::BTreeMap::new();
        for entry in &self.manifest.entries {
            ensure!(
                expected.insert(entry.key.id, entry).is_none(),
                "duplicate export UUID"
            );
        }
        for document in &snapshot.documents {
            let entry = expected
                .remove(&document.key.id)
                .context("unlisted published document")?;
            ensure!(
                entry.key == document.key && entry.document_digest == document.revision,
                "published document differs from export manifest"
            );
        }
        ensure!(expected.is_empty(), "incomplete published corpus");
        let policy = KnowledgeStore::open(&self.paths.policy, false)?;
        ensure!(
            policy.read_control(SELECTION_FILE)?.as_deref() == Some(self.fenced.as_str()),
            "local selection is not the saved fence"
        );
        ensure!(
            policy.read_control("shared.json")?.is_none(),
            "private cutover cannot activate shared transport"
        );
        let registry = IdentityRegistry::open(&self.paths.policy, false)?;
        let identities = registry.read()?.0;
        ensure!(
            identities.corpus_id == self.manifest.corpus_id,
            "prepared corpus identity differs"
        );
        let binding: Binding = serde_json::from_str(&self.fenced)?;
        for (legacy, portable) in &binding.mappings.repos {
            ensure!(
                registry.from_legacy(self.manifest.database_id, *legacy)? == *portable,
                "prepared repository mapping differs"
            );
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    directory: PathBuf,
    version: u32,
    operation: Uuid,
    request: Request,
}

pub struct PrivateJournal {
    store: KnowledgeStore,
    directory: PathBuf,
    intent: Intent,
    bytes: String,
    _lease: File,
}
impl PrivateJournal {
    /// Source leases are already held. Do not wait on another journal holder
    /// who may be waiting for those leases. The intent is fsynced before return.
    pub fn prepare(
        directory: &Path,
        manifest: Manifest,
        recovery: &PairedBackup,
        paths: RecoveryPaths,
    ) -> Result<Self> {
        ensure!(directory.is_absolute(), "absolute cutover journal required");
        let directory = directory
            .parent()
            .context("journal parent required")?
            .canonicalize()?
            .join(directory.file_name().context("journal name required")?);
        recovery.require_outside_sources(&directory)?;
        for path in [
            &paths.corpus,
            &paths.policy,
            &paths.corpus_archive,
            &paths.policy_archive,
        ] {
            ensure!(path.is_absolute(), "absolute cutover paths required");
        }
        let paths = RecoveryPaths {
            corpus: paths.corpus.canonicalize()?,
            policy: paths.policy.canonicalize()?,
            corpus_archive: paths.corpus_archive.canonicalize()?,
            policy_archive: paths.policy_archive.canonicalize()?,
        };
        let policy = KnowledgeStore::open(&paths.policy, false)?;
        let fenced = policy
            .read_control(SELECTION_FILE)?
            .context("prepared fenced local binding required")?;
        let mut binding: Binding = serde_json::from_str(&fenced)?;
        binding.phase = Phase::Okf;
        binding.generation = binding
            .generation
            .checked_add(1)
            .context("generation overflow")?;
        let request = Request {
            corpus_identity: identity(&paths.corpus)?,
            policy_identity: identity(&paths.policy)?,
            manifest,
            paths,
            corpus_revision: recovery.corpus().revision.clone(),
            policy_revision: recovery.policy().revision.clone(),
            fenced,
            selected: serde_json::to_string(&binding)?,
        };
        request.validate(&directory)?;
        request.frozen(recovery)?;
        let store = KnowledgeStore::open(&directory, true)?;
        store.verify_root_path(&directory)?;
        let lease = store
            .try_export_lease()
            .context("cutover journal busy; release source leases before retry")?;
        let intent = if let Some(bytes) = store.read_artifact(INTENT)? {
            let old: Intent = serde_json::from_str(&bytes)?;
            ensure!(
                old.directory == directory
                    && old.version == 1
                    && !old.operation.is_nil()
                    && serde_json::to_value(&old.request)? == serde_json::to_value(&request)?,
                "cutover journal belongs to another request"
            );
            old
        } else {
            Intent {
                directory: directory.clone(),
                version: 1,
                operation: Uuid::new_v4(),
                request,
            }
        };
        let bytes = serde_json::to_string(&intent)?;
        store.retain_artifact(INTENT, &bytes, store.read_artifact(INTENT)?.is_none())?;
        Ok(Self {
            store,
            directory,
            intent,
            bytes,
            _lease: lease,
        })
    }
    pub fn open(directory: &Path) -> Result<Self> {
        let store = KnowledgeStore::open(directory, false)?;
        let directory = directory.canonicalize()?;
        store.verify_root_path(&directory)?;
        let lease = store.operation_lock(".export.lock")?;
        let bytes = store
            .read_artifact(INTENT)?
            .context("cutover journal intent missing")?;
        let intent: Intent = serde_json::from_str(&bytes)?;
        ensure!(
            intent.directory == directory && intent.version == 1 && !intent.operation.is_nil(),
            "invalid cutover journal identity/version"
        );
        intent.request.validate(&directory)?;
        Ok(Self {
            store,
            directory,
            intent,
            bytes,
            _lease: lease,
        })
    }
    pub fn operation(&self) -> Uuid {
        self.intent.operation
    }
    fn revalidate(&self) -> Result<()> {
        self.store.verify_root_path(&self.directory)?;
        ensure!(
            self.store.read_artifact(INTENT)?.as_deref() == Some(&self.bytes),
            "cutover journal changed"
        );
        self.intent.request.validate(&self.directory)
    }
    /// Complete SQL activation then publish the saved local binding. An error
    /// after SQL commit leaves the fence/intent available for the same retry.
    /// Shared transport and multi-host coordination require their own workflow.
    pub async fn activate(&self, pool: &sqlx::PgPool) -> Result<Outcome> {
        self.revalidate()?;
        let request = &self.intent.request;
        let policy = KnowledgeStore::open(&request.paths.policy, false)?;
        let _selection = policy.selection_lease(true)?;
        let current = policy.read_control(SELECTION_FILE)?;
        if current.as_deref() == Some(request.selected.as_str()) {
            // Local publication already happened. Current OKF content may have
            // acknowledged edits; never compare/restore it to the old export.
            let mut tx = pool.begin().await?;
            let recorded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM public.knowledge_forward_receipts WHERE operation_id=$1)")
                .bind(self.intent.operation).fetch_one(&mut *tx).await?;
            ensure!(
                recorded,
                "selected local binding has no SQL activation receipt"
            );
            ensure!(
                forward::activate_on(&mut tx, self.intent.operation, &request.manifest).await?
                    == Outcome::PreviouslyActivated,
                "local activation requires the prior SQL outcome"
            );
            self.revalidate()?;
            tx.commit().await?;
            return Ok(Outcome::PreviouslyActivated);
        }
        ensure!(
            current.as_deref() == Some(request.fenced.as_str()),
            "selection conflicts with saved cutover intent"
        );
        let corpus = KnowledgeStore::open(&request.paths.corpus, false)?;
        let recovery = corpus.resume_selection_pair(
            &policy,
            &request.paths.corpus_archive,
            &request.paths.policy_archive,
            self.intent.operation,
            &request.selected,
        )?;
        request.frozen(&recovery)?;
        let mut tx = pool.begin().await?;
        let outcome =
            forward::activate_on(&mut tx, self.intent.operation, &request.manifest).await?;
        request.frozen(&recovery)?;
        self.revalidate()?;
        tx.commit()
            .await
            .context("SQL activation outcome uncertain; resume the same cutover journal")?;
        // SQL commit necessarily releases the exclusive lease. Reacquire the
        // selected generation before file publication; a concurrent rollback or
        // transition in that gap must leave this local binding fenced.
        let selected = super::guard::selected_transaction(
            pool,
            request.manifest.database_id,
            request.manifest.corpus_id,
            request.manifest.generation + 1,
        )
        .await
        .context(
            "SQL committed but generation changed before local activation; preserve journal",
        )?;
        self.revalidate()?;
        recovery
            .publish_selection(self.intent.operation, &request.fenced, &request.selected)
            .context("SQL committed; local publication needs recovery using the same journal")?;
        self.revalidate()?;
        selected
            .commit()
            .await
            .context("local binding published; resume journal to verify SQL outcome")?;
        Ok(outcome)
    }
}
