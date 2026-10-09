//! Durable rollback intent and receipt-aware SQL apply. This intentionally leaves
//! storage fenced: fleet validation and configuration activation are separate.
use super::{
    document::digest,
    export::Manifest,
    reverse::{self, ApplyOutcome, Candidate, RecoveryEvidence},
    shared::SharedGit,
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

const INTENT: &str = "rollback-intent.json";
const APPLIED: &str = "rollback-applied.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPaths {
    pub corpus: PathBuf,
    pub policy: PathBuf,
    pub corpus_archive: PathBuf,
    pub policy_archive: PathBuf,
}
impl RecoveryPaths {
    fn normalize(&self) -> Result<Self> {
        for path in [
            &self.corpus,
            &self.policy,
            &self.corpus_archive,
            &self.policy_archive,
        ] {
            ensure!(path.is_absolute(), "absolute rollback paths required");
        }
        Ok(Self {
            corpus: self.corpus.canonicalize()?,
            policy: self.policy.canonicalize()?,
            corpus_archive: self.corpus_archive.canonicalize()?,
            policy_archive: self.policy_archive.canonicalize()?,
        })
    }
}
fn identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir(), "rollback source must remain a directory");
    Ok((metadata.dev(), metadata.ino()))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    original: Manifest,
    candidate: Candidate,
    evidence: RecoveryEvidence,
    fenced_generation: i64,
    paths: RecoveryPaths,
    corpus_identity: (u64, u64),
    policy_identity: (u64, u64),
}
impl Request {
    fn verify_directory(&self, directory: &Path) -> Result<()> {
        for path in [
            &self.paths.corpus,
            &self.paths.policy,
            &self.paths.corpus_archive,
            &self.paths.policy_archive,
        ] {
            let path = path.canonicalize()?;
            ensure!(
                !directory.starts_with(&path) && !path.starts_with(directory),
                "journal must be separate from sources and archives"
            );
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.original.version == 1
                && self.candidate.version == 1
                && self.evidence.version == 1,
            "unsupported rollback journal version"
        );
        ensure!(
            self.candidate.database_id == self.original.database_id
                && self.candidate.corpus_id == self.original.corpus_id
                && self.candidate.export_generation == self.original.generation
                && self.fenced_generation > self.original.generation
                && self.candidate.export_digest == digest(&serde_json::to_vec(&self.original)?)
                && self.evidence.candidate_sha256 == digest(&serde_json::to_vec(&self.candidate)?),
            "rollback journal identity/digest mismatch"
        );
        for path in [
            &self.paths.corpus,
            &self.paths.policy,
            &self.paths.corpus_archive,
            &self.paths.policy_archive,
        ] {
            ensure!(
                path.is_absolute(),
                "absolute rollback journal paths required"
            );
        }
        ensure!(
            KnowledgeBackup::verify(&self.paths.corpus_archive)?.revision
                == self.evidence.corpus_revision
                && KnowledgeBackup::verify(&self.paths.policy_archive)?.revision
                    == self.evidence.policy_revision,
            "rollback recovery archive differs from journal"
        );
        Ok(())
    }
    fn verify_roots(&self, recovery: &PairedBackup) -> Result<()> {
        ensure!(
            identity(&self.paths.corpus)? == self.corpus_identity
                && identity(&self.paths.policy)? == self.policy_identity,
            "rollback source directory identity changed"
        );
        recovery.verify_paths(&self.paths.corpus, &self.paths.policy)?;
        ensure!(
            recovery.corpus().revision == self.evidence.corpus_revision
                && recovery.policy().revision == self.evidence.policy_revision,
            "rollback sources differ from retained evidence"
        );
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    operation: Uuid,
    request: Request,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalReceipt {
    version: u32,
    operation: Uuid,
    intent_sha256: String,
}

#[derive(Serialize)]
pub struct Status {
    pub operation: Uuid,
    pub database_id: Uuid,
    pub corpus_id: Uuid,
    pub fenced_generation: i64,
    pub notes: usize,
    pub learnings: usize,
    pub candidate_sha256: String,
    pub shared_commit: Option<String>,
    /// A local hint, never proof of the database's current receipt or rows.
    pub local_apply_recorded: bool,
}
fn read(store: &KnowledgeStore) -> Result<(String, Intent)> {
    let bytes = store
        .read_artifact(INTENT)?
        .context("rollback journal has no durable intent")?;
    let intent: Intent = serde_json::from_str(&bytes)?;
    intent.request.validate()?;
    Ok((bytes, intent))
}
/// Offline inspection does not acquire/create locks, open database configuration,
/// fetch Git, or claim that a local marker proves database application.
pub fn inspect(directory: &Path) -> Result<Status> {
    let store = KnowledgeStore::open(directory, false)?;
    let (bytes, intent) = read(&store)?;
    let local = store
        .read_control(APPLIED)?
        .map(|text| -> Result<bool> {
            let local: LocalReceipt = serde_json::from_str(&text)?;
            ensure!(
                local.version == 1
                    && local.operation == intent.operation
                    && local.intent_sha256 == digest(bytes.as_bytes()),
                "local rollback receipt differs from journal"
            );
            Ok(true)
        })
        .transpose()?
        .unwrap_or(false);
    Ok(Status {
        operation: intent.operation,
        database_id: intent.request.candidate.database_id,
        corpus_id: intent.request.candidate.corpus_id,
        fenced_generation: intent.request.fenced_generation,
        notes: intent.request.candidate.notes.len(),
        learnings: intent.request.candidate.learnings.len(),
        candidate_sha256: intent.request.evidence.candidate_sha256,
        shared_commit: intent.request.evidence.shared_commit,
        local_apply_recorded: local,
    })
}

pub struct Journal {
    store: KnowledgeStore,
    directory: PathBuf,
    intent: Intent,
    bytes: String,
    _lease: File,
}
impl Journal {
    /// Save all retry input and the operation ID with file+directory fsync before
    /// any SQL apply. Repeating identical preparation retains the original UUID.
    pub fn prepare(
        directory: &Path,
        original: Manifest,
        candidate: Candidate,
        recovery: &PairedBackup,
        paths: RecoveryPaths,
        fenced_generation: i64,
        shared_commit: Option<String>,
    ) -> Result<Self> {
        ensure!(
            directory.is_absolute(),
            "absolute rollback journal directory required"
        );
        let directory = directory
            .parent()
            .context("journal parent required")?
            .canonicalize()?
            .join(directory.file_name().context("journal filename required")?);
        let paths = paths.normalize()?;
        recovery.require_outside_sources(&directory)?;
        for archive in [&paths.corpus_archive, &paths.policy_archive] {
            ensure!(
                !directory.starts_with(archive) && !archive.starts_with(&directory),
                "journal and archives must be separate"
            );
        }
        let request = Request {
            version: 1,
            evidence: RecoveryEvidence::new(&candidate, recovery, shared_commit)?,
            corpus_identity: identity(&paths.corpus)?,
            policy_identity: identity(&paths.policy)?,
            original,
            candidate,
            paths,
            fenced_generation,
        };
        request.validate()?;
        request.verify_roots(recovery)?;
        request.verify_directory(&directory)?;
        let store = KnowledgeStore::open(&directory, true)?;
        store.verify_root_path(&directory)?;
        let lease = store
            .try_export_lease()
            .context("rollback journal is busy; release source leases before retrying")?;
        if store.read_artifact(INTENT)?.is_some() {
            let (bytes, intent) = read(&store)?;
            ensure!(
                serde_json::to_value(&intent.request)? == serde_json::to_value(&request)?,
                "rollback journal belongs to a different request"
            );
            return Ok(Self {
                store,
                directory,
                intent,
                bytes,
                _lease: lease,
            });
        }
        let intent = Intent {
            operation: Uuid::new_v4(),
            request,
        };
        let bytes = serde_json::to_string(&intent)?;
        store.retain_artifact(INTENT, &bytes, true)?;
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
        let (bytes, intent) = read(&store)?;
        intent.request.verify_directory(&directory)?;
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
        self.intent.request.verify_directory(&self.directory)?;
        ensure!(
            self.store.read_artifact(INTENT)?.as_deref() == Some(&self.bytes),
            "rollback journal changed during operation"
        );
        self.intent.request.validate()
    }
    /// Reacquire local selection/writer leases, revalidate current evidence and
    /// apply using the saved receipt identity. Caller must quiesce external editors
    /// and all remote hosts. Apply requires fenced SQL and preserves local selection;
    /// it does not perform a backend transition or certify fleet compatibility.
    pub async fn apply(
        &self,
        pool: &sqlx::PgPool,
        transport: Option<&SharedGit>,
    ) -> Result<ApplyOutcome> {
        self.revalidate()?;
        let request = &self.intent.request;
        let policy = KnowledgeStore::open(&request.paths.policy, false)?;
        let _selection = policy.selection_lease(true)?;
        let corpus = KnowledgeStore::open(&request.paths.corpus, false)?;
        let recovery = corpus.resume_pair_retained(
            &policy,
            &request.paths.corpus_archive,
            &request.paths.policy_archive,
        )?;
        request.verify_roots(&recovery)?;
        let mut tx = pool.begin().await?;
        let current = match (&request.evidence.shared_commit, transport) {
            (None, None) => {
                reverse::capture_recovery_on(
                    &mut tx,
                    &request.original,
                    &recovery,
                    request.fenced_generation,
                )
                .await?
            }
            (Some(expected), Some(transport)) => {
                let current = reverse::capture_shared_recovery_on(
                    &mut tx,
                    &request.original,
                    transport,
                    &recovery,
                    request.fenced_generation,
                )
                .await?;
                ensure!(
                    &current.commit == expected,
                    "shared rollback commit differs from journal"
                );
                current.candidate
            }
            _ => anyhow::bail!("rollback transport differs from journal"),
        };
        ensure!(
            serde_json::to_value(&current)? == serde_json::to_value(&request.candidate)?,
            "current rollback candidate differs from journal"
        );
        let outcome = reverse::apply_once_on(
            &mut tx,
            self.intent.operation,
            &request.candidate,
            &request.evidence,
            request.fenced_generation,
        )
        .await?;
        self.revalidate()?;
        request.verify_roots(&recovery)?;
        if let (Some(commit), Some(transport)) = (&request.evidence.shared_commit, transport) {
            transport.verify_recovery(&recovery, commit)?;
        }
        tx.commit()
            .await
            .context("rollback commit outcome uncertain; retain and resume the same journal")?;
        let receipt = LocalReceipt {
            version: 1,
            operation: self.intent.operation,
            intent_sha256: digest(self.bytes.as_bytes()),
        };
        self.store
            .retain_artifact(APPLIED, &serde_json::to_string(&receipt)?, false)
            .context(
                "SQL apply committed but local receipt was not recorded; resume the same journal",
            )?;
        Ok(outcome)
    }
}
