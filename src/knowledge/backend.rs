//! One service contract over private files and confirmed shared Git snapshots.
use super::{
    document::{Document, digest},
    shared::{Change, SharedGit, Snapshot as RemoteSnapshot},
    store::{
        Candidate, Candidates, ExpectedRevision, Key, KnowledgeStore, RevisionedDocument, Snapshot,
    },
};
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::RwLock,
};
use uuid::Uuid;

pub(super) enum Backend {
    Private(KnowledgeStore),
    Shared(Shared),
}
pub(super) struct Shared {
    transport: SharedGit,
    root: PathBuf,
    current: RwLock<View>,
}
struct View {
    path: PathBuf,
    store: KnowledgeStore,
    snapshot: RemoteSnapshot,
}
impl Drop for View {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
impl View {
    fn create(root: &Path, snapshot: RemoteSnapshot) -> Result<Self> {
        let path = root.join(format!(".view-{}", Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        let view = Self {
            store: KnowledgeStore::open(&path, false)?,
            path,
            snapshot,
        };
        for (name, bytes) in &view.snapshot.files {
            // Transport validated relative, non-hidden, bounded paths and modes.
            let file = view.path.join(name);
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(file.parent().unwrap())?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&file)?;
            output.write_all(bytes)?;
        }
        // Views are disposable copies of durable Git objects, not acknowledged
        // content writes. No fsync or publication marker is required for them.
        Ok(view)
    }
}
impl Backend {
    pub fn shared(root: &Path, transport: SharedGit, refresh: bool) -> Result<Self> {
        let cached = if !refresh {
            transport.cached().ok().filter(|s| s.fresh(Utc::now()))
        } else {
            None
        };
        let snapshot = if let Some(snapshot) = cached {
            snapshot
        } else {
            match transport.refresh() {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    eprintln!(
                        "knowledge: shared refresh unavailable; browsing last confirmed snapshot"
                    );
                    let mut snapshot = transport.cached()?;
                    transport.invalidate_cached(&snapshot)?;
                    // A new explicit/session entry cannot inherit old authority
                    // after its mandatory refresh failed (including revoked access).
                    snapshot.is_current = false;
                    snapshot
                }
            }
        };
        Ok(Self::Shared(Shared {
            current: RwLock::new(View::create(root, snapshot)?),
            root: root.to_owned(),
            transport,
        }))
    }
    pub fn sync(&self, confirm_pending: bool) -> Result<String> {
        let Self::Shared(shared) = self else {
            anyhow::bail!("no shared Git corpus configured");
        };
        if confirm_pending {
            shared.transport.confirm_pending()?;
        }
        let snapshot = shared.transport.refresh()?;
        let commit = snapshot.commit.clone();
        *shared.current.write().unwrap() = View::create(&shared.root, snapshot)?;
        Ok(commit)
    }
    fn read<T>(&self, f: impl FnOnce(&KnowledgeStore) -> T) -> T {
        match self {
            Self::Private(store) => f(store),
            Self::Shared(shared) => f(&shared.current.read().unwrap().store),
        }
    }
    /// Automatic instructions cannot use a stale or superseded remote snapshot.
    pub fn fresh(&self, now: DateTime<Utc>) -> Result<()> {
        if let Self::Shared(shared) = self {
            if !shared.current.read().unwrap().snapshot.fresh(now) {
                let snapshot = shared.transport.refresh()?;
                ensure!(snapshot.fresh(Utc::now()), "shared snapshot is stale");
                *shared.current.write().unwrap() = View::create(&shared.root, snapshot)?;
            }
        }
        Ok(())
    }
    pub fn find(&self, id: Uuid) -> Result<Option<RevisionedDocument>> {
        self.read(|s| s.find(id))
    }
    pub fn snapshot(&self) -> Snapshot {
        self.read(KnowledgeStore::snapshot)
    }
    pub fn candidates(&self) -> Candidates {
        self.read(KnowledgeStore::candidates)
    }
    pub fn load_candidate(&self, row: &Candidate) -> Result<Option<RevisionedDocument>> {
        self.read(|s| s.load_candidate(row))
    }
    pub fn revalidate_selected(&self, selected: &[RevisionedDocument]) -> Snapshot {
        self.read(|s| s.revalidate_selected(selected))
    }
    fn apply(shared: &Shared, changes: Vec<Change>) -> Result<()> {
        shared.transport.change(&changes)?;
        // Remote reachability already established the acknowledgment. Keep that
        // success even if rebuilding an optional local view fails afterward.
        match shared
            .transport
            .cached()
            .and_then(|snapshot| View::create(&shared.root, snapshot))
        {
            Ok(view) => *shared.current.write().unwrap() = view,
            Err(_) => {
                shared.current.write().unwrap().snapshot.is_current = false;
                eprintln!("knowledge: published; local shared view unavailable until refresh")
            }
        }
        Ok(())
    }
    pub fn put(
        &self,
        document: &Document,
        expected: ExpectedRevision<'_>,
    ) -> Result<RevisionedDocument> {
        match self {
            Self::Private(store) => store.put(document, expected),
            Self::Shared(shared) => {
                let key = Key::from_document(document)?;
                let bytes = document.serialize()?.into_bytes();
                Document::parse(std::str::from_utf8(&bytes)?)?;
                let revision = digest(&bytes);
                let expected = match expected {
                    ExpectedRevision::Absent => None,
                    ExpectedRevision::Digest(d) => Some(d.to_owned()),
                };
                Self::apply(
                    shared,
                    vec![Change {
                        path: key.relative_path().to_str().unwrap().to_owned(),
                        expected,
                        replacement: Some(bytes),
                    }],
                )?;
                Ok(RevisionedDocument {
                    key,
                    revision,
                    document: document.clone(),
                })
            }
        }
    }
    pub fn delete(&self, key: Key, expected: &str) -> Result<()> {
        match self {
            Self::Private(store) => store.delete(key, expected),
            Self::Shared(shared) => Self::apply(
                shared,
                vec![Change {
                    path: key.relative_path().to_str().unwrap().to_owned(),
                    expected: Some(expected.to_owned()),
                    replacement: None,
                }],
            ),
        }
    }
    pub fn move_document(
        &self,
        old: Key,
        document: &Document,
        expected: &str,
    ) -> Result<RevisionedDocument> {
        match self {
            Self::Private(store) => store.move_document(old, document, expected),
            Self::Shared(shared) => {
                let key = Key::from_document(document)?;
                ensure!(
                    key.id == old.id && key.kind == old.kind && key.repo != old.repo,
                    "invalid shared scope move"
                );
                let bytes = document.serialize()?.into_bytes();
                let revision = digest(&bytes);
                Self::apply(
                    shared,
                    vec![
                        Change {
                            path: old.relative_path().to_str().unwrap().to_owned(),
                            expected: Some(expected.to_owned()),
                            replacement: None,
                        },
                        Change {
                            path: key.relative_path().to_str().unwrap().to_owned(),
                            expected: None,
                            replacement: Some(bytes),
                        },
                    ],
                )?;
                Ok(RevisionedDocument {
                    key,
                    revision,
                    document: document.clone(),
                })
            }
        }
    }
}
