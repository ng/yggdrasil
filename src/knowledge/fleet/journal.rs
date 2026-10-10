//! Private durable coordinator request. Holding this journal does not establish
//! participant authentication or authorize any authority transition.
use super::plan::ValidatedPlan;
use crate::knowledge::store::KnowledgeStore;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

mod activation;
pub use activation::ActivationReceipt;
mod publication;
pub use publication::Publication;

const FILE: &str = "fleet-intent.json";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32,
    directory: PathBuf,
    directory_identity: (u64, u64),
    request_sha256: String,
    request: String,
}

pub struct Journal {
    store: KnowledgeStore,
    intent: Intent,
    bytes: String,
    plan: ValidatedPlan,
    _lease: File,
}
fn target(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "absolute fleet journal path required");
    Ok(path
        .parent()
        .context("journal parent required")?
        .canonicalize()?
        .join(
            path.file_name()
                .context("journal directory name required")?,
        ))
}
fn identity(path: &Path) -> Result<(u64, u64)> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir(),
        "fleet journal must be a directory, not a symlink"
    );
    Ok((meta.dev(), meta.ino()))
}
impl Journal {
    /// Persist the exact request before registration or participant preparation.
    /// A conflicting existing journal is never rewritten. The caller must choose
    /// a directory separate from local deployment/corpus/policy/backup roots.
    pub fn prepare(directory: &Path, plan: ValidatedPlan) -> Result<Self> {
        let directory = target(directory)?;
        let store = KnowledgeStore::open(&directory, true)?;
        let lease = store.try_export_lease()?;
        store.verify_root_path(&directory)?;
        let intent = Intent {
            version: 1,
            directory_identity: identity(&directory)?,
            directory,
            request_sha256: plan.registration().request_sha256.clone(),
            request: plan.bytes().to_owned(),
        };
        let bytes = serde_json::to_string(&intent)?;
        store.retain_artifact(FILE, &bytes, true)?;
        let result = Self {
            store,
            intent,
            bytes,
            plan,
            _lease: lease,
        };
        result.verify()?;
        Ok(result)
    }
    /// Require the independently retained registration digest. The journal's own
    /// checksum alone cannot establish that it is the operation being resumed.
    pub fn resume(directory: &Path, request_sha256: &str) -> Result<Self> {
        let directory = target(directory)?;
        let store = KnowledgeStore::open(&directory, false)?;
        let lease = store.try_export_lease()?;
        store.verify_root_path(&directory)?;
        let bytes = store
            .read_artifact(FILE)?
            .context("fleet journal intent missing")?;
        let intent: Intent = serde_json::from_str(&bytes)?;
        ensure!(
            intent.version == 1
                && intent.directory == directory
                && intent.directory_identity == identity(&directory)?
                && intent.request_sha256 == request_sha256,
            "fleet journal identity or expected request changed"
        );
        let plan = ValidatedPlan::parse(&intent.request)?;
        ensure!(
            plan.registration().request_sha256 == request_sha256,
            "fleet request digest changed"
        );
        let result = Self {
            store,
            intent,
            bytes,
            plan,
            _lease: lease,
        };
        result.verify()?;
        Ok(result)
    }
    /// Reserve and prepare every declared participant. Partial success remains
    /// journaled; retry revalidates every host instead of trusting cached receipts.
    /// This does not fence the database or publish/activate shared knowledge.
    pub async fn prepare_hosts(
        &self,
        config: &crate::config::database::DeploymentConfig,
        pool: &sqlx::PgPool,
        ssh_identity: Option<&Path>,
    ) -> Result<String> {
        self.source_backup(config)?;
        self.plan.registration().register(pool).await?;
        self.visit_hosts(super::protocol::Action::PrepareSql, ssh_identity)
            .await
    }
    fn source_backup(
        &self,
        config: &crate::config::database::DeploymentConfig,
    ) -> Result<crate::knowledge::source_backup::SourceBackup> {
        use crate::knowledge::source_backup::SourceBackup;
        let plan = self.plan()?.plan();
        for path in [
            &config.data_dir,
            &config.knowledge_dir,
            &config.knowledge_policy_dir,
            &plan.source_backup.path,
        ] {
            let path = target(path)?;
            ensure!(
                !path.starts_with(&self.intent.directory)
                    && !self.intent.directory.starts_with(&path),
                "coordinator journal overlaps deployment or backup paths"
            );
        }
        let backup = SourceBackup::open(
            &plan.source_backup.path,
            plan.mappings.database_id,
            plan.source_generation,
        )?;
        ensure!(
            backup.digest() == plan.source_backup.manifest_sha256,
            "coordinator source backup digest changed"
        );
        backup.verify()?;
        backup.verify_configuration(config)?;
        Ok(backup)
    }
    fn verify_prepared(&self, expected: &str) -> Result<()> {
        self.verify()?;
        let bytes = self
            .store
            .read_artifact("fleet-prepared.json")?
            .context("complete fleet preparation evidence missing")?;
        ensure!(
            crate::knowledge::document::digest(bytes.as_bytes()) == expected,
            "complete fleet preparation evidence changed"
        );
        Ok(())
    }
    /// Recontact every host before taking the exclusive SQL generation lease.
    /// On resume, inspection requires this operation's committed fleet fence.
    /// No SSH call runs under the exclusive database lease.
    pub async fn fence_hosts(
        &self,
        config: &crate::config::database::DeploymentConfig,
        pool: &sqlx::PgPool,
        ssh_identity: Option<&Path>,
    ) -> Result<i64> {
        let backup = self.source_backup(config)?;
        let registration = self.plan()?.registration();
        let prepared = if super::transition::has_fence(registration, pool).await? {
            self.visit_hosts(super::protocol::Action::InspectSql, ssh_identity)
                .await?
        } else {
            self.prepare_hosts(config, pool, ssh_identity).await?
        };
        super::transition::fence(registration, pool, &backup, &prepared, || {
            self.verify_prepared(&prepared)
        })
        .await
    }
    /// Produce a private complete export for this fleet fence. The returned
    /// manifest and retained receipt do not authorize Git publication/activation.
    pub async fn stage_hosts(
        &self,
        config: &crate::config::database::DeploymentConfig,
        pool: &sqlx::PgPool,
        ssh_identity: Option<&Path>,
    ) -> Result<crate::knowledge::export::Manifest> {
        use crate::knowledge::{document::digest, export};
        self.fence_hosts(config, pool, ssh_identity).await?;
        let backup = self.source_backup(config)?;
        let plan = self.plan()?.plan();
        let prepared = self
            .store
            .read_artifact("fleet-prepared.json")?
            .context("complete fleet preparation evidence missing")?;
        let prepared_sha256 = digest(prepared.as_bytes());
        let staging = self.intent.directory.join("stage");
        let manifest = export::stage(pool, &plan.mappings, &staging).await?;
        super::transition::with_fenced_source(
            self.plan.registration(),
            pool,
            &backup,
            &prepared_sha256,
            || {
                self.verify_prepared(&prepared_sha256)?;
                ensure!(
                    export::verify(&staging)? == manifest
                        && manifest.database_id == plan.mappings.database_id
                        && manifest.generation == plan.source_generation + 1
                        && manifest.corpus_id == plan.mappings.corpus_id
                        && manifest.mappings == serde_json::to_value(&plan.mappings)?,
                    "staged export differs from the owned fleet source"
                );
                let bytes = self.export_bytes(&manifest, &prepared_sha256, backup.digest())?;
                self.store
                    .retain_artifact("fleet-export.json", &bytes, false)?;
                self.verify_prepared(&prepared_sha256)?;
                Ok(manifest)
            },
        )
        .await
    }
    fn export_bytes(
        &self,
        manifest: &crate::knowledge::export::Manifest,
        prepared: &str,
        backup: &str,
    ) -> Result<String> {
        Ok(serde_json::to_string(&serde_json::json!({
            "version":1, "operation":self.plan()?.plan().operation,
            "request_sha256":self.intent.request_sha256,
            "prepared_sha256":prepared, "source_backup_sha256":backup,
            "manifest":manifest,
        }))?)
    }
    /// Return only this operation's still-fenced SQL source to generation +2,
    /// then reconcile hosts. After activation, use current-state rollback instead.
    pub async fn abort_hosts(
        &self,
        config: &crate::config::database::DeploymentConfig,
        pool: &sqlx::PgPool,
        ssh_identity: Option<&Path>,
    ) -> Result<String> {
        let backup = self.source_backup(config)?;
        let bytes = self
            .store
            .read_artifact("fleet-prepared.json")?
            .context("complete fleet preparation evidence missing")?;
        let prepared = crate::knowledge::document::digest(bytes.as_bytes());
        super::transition::abort(
            self.plan()?.registration(),
            pool,
            &backup,
            &prepared,
            || self.verify_prepared(&prepared),
            || self.verify_publication_for_abort(),
        )
        .await?;
        self.visit_hosts(super::protocol::Action::AbortSql, ssh_identity)
            .await
    }
    /// Cancel the SQL source generation first, then reconcile all declared hosts,
    /// including hosts which never prepared. No cached receipt skips a host.
    pub async fn cancel_hosts(
        &self,
        pool: &sqlx::PgPool,
        ssh_identity: Option<&Path>,
    ) -> Result<String> {
        self.plan()?.registration().cancel(pool).await?;
        self.visit_hosts(super::protocol::Action::CancelSql, ssh_identity)
            .await
    }
    async fn visit_hosts(
        &self,
        action: super::protocol::Action,
        ssh_identity: Option<&Path>,
    ) -> Result<String> {
        use crate::knowledge::document::digest;
        let phase = match action {
            super::protocol::Action::PrepareSql | super::protocol::Action::InspectSql => "prepared",
            super::protocol::Action::AbortSql => "aborted",
            super::protocol::Action::ReadySql | super::protocol::Action::FinalizeSql => {
                anyhow::bail!("readiness requires publication-bound dispatch")
            }
            super::protocol::Action::CancelSql => "cancelled",
        };
        let mut records = Vec::new();
        let mut failures = Vec::new();
        for participant in &self.plan()?.plan().participants {
            let response =
                match super::protocol::call(self, participant.id, action, ssh_identity).await {
                    Ok(response) => response,
                    Err(error)
                        if matches!(
                            action,
                            super::protocol::Action::CancelSql | super::protocol::Action::AbortSql
                        ) =>
                    {
                        // SQL recovery has already committed. Reconcile later
                        // reachable hosts even if this host needs a subsequent retry.
                        // A changed local journal still stops the operation immediately.
                        self.verify()?;
                        failures.push(format!("{}: {error:#}", participant.id));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            self.verify()?;
            // Inspection revalidates the original preparation bytes; its fresh
            // nonce belongs only to the live authenticated exchange.
            let record_action = if action == super::protocol::Action::InspectSql {
                super::protocol::Action::PrepareSql
            } else {
                action
            };
            let record = serde_json::json!({"version":1,"operation":self.plan.plan().operation,
                "request_sha256":self.intent.request_sha256,"action":record_action,"receipt":response.preparation()});
            let bytes = serde_json::to_string(&record)?;
            self.store.retain_artifact(
                &format!("{phase}-{}.json", participant.id),
                &bytes,
                false,
            )?;
            records.push(record);
        }
        self.verify()?;
        ensure!(
            failures.is_empty(),
            "fleet {phase} reconciliation incomplete; retry the same journal: {}",
            failures.join("; ")
        );
        let bytes = serde_json::to_string(&records)?;
        self.store
            .retain_artifact(&format!("fleet-{phase}.json"), &bytes, false)?;
        Ok(digest(bytes.as_bytes()))
    }

    fn verify(&self) -> Result<()> {
        self.store.verify_root_path(&self.intent.directory)?;
        ensure!(
            identity(&self.intent.directory)? == self.intent.directory_identity,
            "fleet journal directory replaced"
        );
        ensure!(
            self.store.read_artifact(FILE)?.as_deref() == Some(self.bytes.as_str()),
            "fleet journal intent changed"
        );
        Ok(())
    }
    pub fn plan(&self) -> Result<&ValidatedPlan> {
        self.verify()?;
        Ok(&self.plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan() -> ValidatedPlan {
        ValidatedPlan::parse(&super::super::plan::tests::fixture().to_string()).unwrap()
    }
    fn resume_after_release(directory: &Path, hash: &str) -> Journal {
        // Other tests launch subprocesses concurrently. Permit a short WouldBlock
        // after dropping our owner, while inherited descriptors close at exec.
        // Do not retry integrity errors or relax the live-owner exclusion test.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match Journal::resume(directory, hash) {
                Ok(journal) => return journal,
                Err(error)
                    if std::time::Instant::now() < deadline
                        && error.chain().any(|cause| {
                            cause
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|io| io.kind() == std::io::ErrorKind::WouldBlock)
                        }) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("journal resume after owner release failed: {error:#}"),
            }
        }
    }
    #[test]
    fn persists_exact_request_excludes_competing_owner_and_resumes() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("journal");
        let original = plan();
        let bytes = original.bytes().to_owned();
        let hash = original.registration().request_sha256.clone();
        let journal = Journal::prepare(&dir, original).unwrap();
        assert!(Journal::resume(&dir, &hash).is_err());
        assert!(Journal::prepare(&dir, plan()).is_err());
        assert_eq!(journal.plan().unwrap().bytes(), bytes);
        drop(journal);
        let journal = resume_after_release(&dir, &hash);
        assert_eq!(journal.plan().unwrap().bytes(), bytes);
        drop(journal);
        assert!(Journal::resume(&dir, &"0".repeat(64)).is_err());
        assert!(Journal::prepare(&dir, plan()).is_err());
        Journal::prepare(&dir, ValidatedPlan::parse(&bytes).unwrap()).unwrap();
    }
    #[test]
    fn refuses_copied_directory_changed_intent_and_replaced_live_root() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("journal");
        let journal = Journal::prepare(&dir, plan()).unwrap();
        let hash = journal
            .plan()
            .unwrap()
            .registration()
            .request_sha256
            .clone();
        let saved = std::fs::read(dir.join(FILE)).unwrap();
        std::fs::write(dir.join(FILE), b"{}").unwrap();
        assert!(journal.plan().is_err());
        std::fs::write(dir.join(FILE), &saved).unwrap();
        let moved = temp.path().join("moved");
        std::fs::rename(&dir, &moved).unwrap();
        assert!(journal.plan().is_err());
        drop(journal);
        assert!(Journal::resume(&moved, &hash).is_err());
        KnowledgeStore::open(&dir, true).unwrap();
        std::fs::write(dir.join(FILE), saved).unwrap();
        assert!(Journal::resume(&dir, &hash).is_err());
    }
    #[test]
    fn child_journal_owner() {
        let Some(directory) = std::env::var_os("YGG_FLEET_JOURNAL_TEST_CHILD") else {
            return;
        };
        let directory = PathBuf::from(directory);
        let bytes = std::fs::read_to_string(directory.join("request.json")).unwrap();
        let journal = Journal::prepare(
            &directory.join("journal"),
            ValidatedPlan::parse(&bytes).unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory.join("ready"),
            journal
                .plan()
                .unwrap()
                .registration()
                .request_sha256
                .as_bytes(),
        )
        .unwrap();
        // The parent owns this pipe and kills us while the exclusive lease is held.
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        drop(journal);
    }

    #[test]
    fn killed_coordinator_releases_lease_and_retains_exact_request() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let request = plan();
        let hash = request.registration().request_sha256.clone();
        std::fs::write(temp.path().join("request.json"), request.bytes()).unwrap();
        let mut child = Child(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "knowledge::fleet::journal::tests::child_journal_owner",
                    "--nocapture",
                ])
                .env("YGG_FLEET_JOURNAL_TEST_CHILD", temp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        let ready = temp.path().join("ready");
        while !ready.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before retaining journal"
            );
            assert!(
                Instant::now() < deadline,
                "child journal preparation timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let directory = temp.path().join("journal");
        assert!(Journal::resume(&directory, &hash).is_err());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let resumed = Journal::resume(&directory, &hash).unwrap();
        assert_eq!(resumed.plan().unwrap().bytes(), request.bytes());
    }

    #[test]
    fn refuses_nonempty_directory_without_intent() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("journal");
        KnowledgeStore::open(&dir, true).unwrap();
        std::fs::write(dir.join("unrelated"), b"retain").unwrap();
        assert!(Journal::prepare(&dir, plan()).is_err());
        assert_eq!(std::fs::read(dir.join("unrelated")).unwrap(), b"retain");
        assert!(!dir.join(FILE).exists());
    }
}
