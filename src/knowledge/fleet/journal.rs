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
        let journal = Journal::resume(&dir, &hash).unwrap();
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
