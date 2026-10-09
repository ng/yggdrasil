//! Private local bundles with cooperative locking and conditional durable writes.
//! Unix directory descriptors anchor operations: a symlink swap cannot redirect
//! reads, writes or deletion outside the configured corpus.
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
};

use anyhow::{Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

mod backup;
pub use backup::{BackupEntry, KnowledgeBackup, PairedBackup};
mod index;
pub(crate) use index::{Candidate, Candidates};
mod moves;
use uuid::Uuid;

use super::document::{Document, MAX_DOCUMENT_BYTES, Scope, digest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Note,
    Learning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub repo: Option<Uuid>,
    pub kind: Kind,
    pub id: Uuid,
}

impl Key {
    pub fn from_document(doc: &Document) -> Result<Self> {
        let profile = doc
            .profile()?
            .ok_or_else(|| anyhow::anyhow!("Yggdrasil profile required"))?;
        let kind = match doc.document_type()? {
            "Note" => Kind::Note,
            "Engineering Rule" => Kind::Learning,
            _ => bail!("generic OKF documents cannot be written as Yggdrasil notes/rules"),
        };
        Ok(Self {
            repo: match profile.scope {
                Scope::Global => None,
                Scope::Repo => profile.repo,
            },
            kind,
            id: profile.id,
        })
    }

    pub fn relative_path(self) -> PathBuf {
        let scope = match self.repo {
            Some(repo) => PathBuf::from("repos").join(repo.to_string()),
            None => PathBuf::from("global"),
        };
        scope
            .join(match self.kind {
                Kind::Note => "notes",
                Kind::Learning => "learnings",
            })
            .join(format!("{}.md", self.id))
    }
}

#[derive(Debug, Clone)]
pub struct RevisionedDocument {
    pub key: Key,
    /// Digest of exact file bytes, distinct from the activation digest.
    pub revision: String,
    pub document: Document,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub documents: Vec<RevisionedDocument>,
    pub diagnostics: Vec<String>,
}

#[derive(Default)]
struct Inventory {
    keys: Vec<Key>,
    diagnostics: Vec<String>,
    incomplete: bool,
}

pub enum ExpectedRevision<'a> {
    Absent,
    Digest(&'a str),
}

pub struct KnowledgeStore {
    root: File,
}

fn child(dir: &File, name: &str, flags: i32, mode: u32) -> std::io::Result<File> {
    let name = CString::new(name)?;
    // All names are single components, chosen by this module. O_NOFOLLOW is
    // applied even to the lock and temporary files, not just documents.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a fresh owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Open a persistent lock inode without replacing another participant's lease.
/// Separate existing opens from exclusive creation: concurrent O_CREAT opens
/// can transiently fail with ENOENT on macOS during first publication.
fn lock_file(dir: &File, name: &str) -> std::io::Result<File> {
    for _ in 0..8 {
        match child(dir, name, libc::O_RDWR, 0) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match child(
            dir,
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(file) => return Ok(file),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other(
        "knowledge lock changed during creation",
    ))
}

fn directory(dir: &File, name: &str, create: bool) -> std::io::Result<File> {
    if create {
        let name_c = CString::new(name)?;
        let result = unsafe { libc::mkdirat(dir.as_raw_fd(), name_c.as_ptr(), 0o700) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(err);
            }
        } else {
            dir.sync_all()?;
        }
    }
    child(dir, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
}

fn unlink(dir: &File, name: &str) -> std::io::Result<()> {
    let name = CString::new(name)?;
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Read directory names from the held descriptor, never reopening a pathname.
fn names(dir: &File) -> Result<Vec<String>> {
    names_limited(dir, usize::MAX)
}

fn names_limited(dir: &File, limit: usize) -> Result<Vec<String>> {
    // Opening "." gives an independent directory offset (dup would share it).
    let reader = child(dir, ".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    use std::os::fd::IntoRawFd;
    let fd = reader.into_raw_fd();
    let raw = unsafe { libc::fdopendir(fd) };
    if raw.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(err.into());
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let owned = Directory(raw);
    let mut result = Vec::new();
    loop {
        // readdir returns NULL both on EOF and error; reset errno first.
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(owned.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error.into());
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if name != "." && name != ".." {
            ensure!(result.len() < limit, "directory exceeds entry limit");
            result.push(name);
        }
    }
    result.sort();
    Ok(result)
}

impl KnowledgeStore {
    pub(super) fn verify_root_path(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let expected = self.root.metadata()?;
        let actual = std::fs::symlink_metadata(path)?;
        ensure!(
            actual.is_dir() && actual.dev() == expected.dev() && actual.ino() == expected.ino(),
            "knowledge directory path changed"
        );
        Ok(())
    }

    /// Creating a corpus is explicit. Opening an absent corpus never creates it.
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
        if create {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = root.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "knowledge root must be owned by current user"
        );
        if create {
            // Persist entries for any ancestors created by recursive mkdir.
            let canonical = path.canonicalize()?;
            for ancestor in canonical.ancestors().skip(1) {
                File::open(ancestor)?.sync_all()?;
            }
        }
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "knowledge root must be private (chmod 700)"
        );
        Ok(Self { root })
    }

    pub(super) fn private_child(&self, name: &str) -> Result<Self> {
        ensure!(
            !name.is_empty()
                && name != "."
                && name != ".."
                && !name.contains('/')
                && !name.contains('\\'),
            "invalid private child name"
        );
        let root = directory(&self.root, name, true)?;
        use std::os::unix::fs::MetadataExt;
        let metadata = root.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "knowledge session directory must be private and owned"
        );
        Ok(Self { root })
    }

    fn lock(&self) -> Result<File> {
        self.operation_lock(".writer.lock")
    }

    /// Preparation already holds source leases, so it must never wait for a
    /// journal owner that may be acquiring those same source leases.
    pub(super) fn try_export_lease(&self) -> Result<File> {
        let file = lock_file(&self.root, ".export.lock")?;
        ensure!(
            file.metadata()?.is_file(),
            "knowledge lock is not a regular file"
        );
        file.try_lock_exclusive()?;
        Ok(file)
    }

    pub(super) fn operation_lock(&self, name: &str) -> Result<File> {
        ensure!(
            !name.contains('/') && !name.contains('\\') && name != "..",
            "invalid lock filename"
        );
        let file = lock_file(&self.root, name)?;
        ensure!(
            file.metadata()?.is_file(),
            "knowledge lock is not a regular file"
        );
        file.lock_exclusive()?;
        Ok(file)
    }

    /// Runtime readers hold a shared lease; cutover/rollback takes the exclusive
    /// lease before changing selection. Separate from document writer locks.
    pub(super) fn selection_lease(&self, exclusive: bool) -> Result<File> {
        let file = lock_file(&self.root, ".selection.lock")?;
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "invalid knowledge selection lease"
        );
        if exclusive {
            file.lock_exclusive()?;
        } else {
            FileExt::lock_shared(&file)?;
        }
        Ok(file)
    }

    /// Export artifacts are bounded independently of single OKF documents.
    pub(super) fn read_artifact(&self, name: &str) -> Result<Option<String>> {
        ensure!(
            !name.contains('/') && !name.contains('\\') && name != "..",
            "invalid artifact filename"
        );
        Self::read_limited(&self.root, name, 64 * 1024 * 1024)
    }

    pub(super) fn retain_artifact(&self, name: &str, text: &str, initializing: bool) -> Result<()> {
        ensure!(
            text.len() <= 64 * 1024 * 1024,
            "export artifact exceeds 64 MiB limit"
        );
        let _lock = self.lock()?;
        if let Some(current) = self.read_artifact(name)? {
            ensure!(
                current == text,
                "export artifact conflict; preserve staging and choose a new destination"
            );
        } else {
            if initializing {
                ensure!(
                    names(&self.root)?.iter().all(|n| n == ".writer.lock"
                        || n == ".export.lock"
                        || n.strip_prefix('.')
                            .and_then(|s| s.strip_suffix(".tmp"))
                            .is_some_and(|s| Uuid::parse_str(s).is_ok())),
                    "staging directory is not empty and has no matching export intent"
                );
            }
            Self::replace_at(&self.root, name, text)?;
        }
        Ok(())
    }

    fn parent(&self, key: Key, create: bool) -> Result<File> {
        let scope = match key.repo {
            Some(repo) => directory(
                &directory(&self.root, "repos", create)?,
                &repo.to_string(),
                create,
            )?,
            None => directory(&self.root, "global", create)?,
        };
        Ok(directory(
            &scope,
            match key.kind {
                Kind::Note => "notes",
                Kind::Learning => "learnings",
            },
            create,
        )?)
    }

    fn read_at(parent: &File, name: &str) -> Result<Option<String>> {
        Self::read_limited(parent, name, MAX_DOCUMENT_BYTES)
    }

    fn read_limited(parent: &File, name: &str, limit: usize) -> Result<Option<String>> {
        let file = match child(parent, name, libc::O_RDONLY, 0) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            file.metadata()?.is_file(),
            "knowledge document is not a regular file"
        );
        let mut text = String::new();
        file.take((limit + 1) as u64).read_to_string(&mut text)?;
        ensure!(text.len() <= limit, "knowledge document exceeds byte limit");
        Ok(Some(text))
    }

    pub fn get(&self, key: Key) -> Result<Option<RevisionedDocument>> {
        self.recover_move()?;
        self.get_unchecked(key)
    }

    fn get_unchecked(&self, key: Key) -> Result<Option<RevisionedDocument>> {
        let parent = match self.parent(key, false) {
            Ok(parent) => parent,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let Some(text) = Self::read_at(&parent, &format!("{}.md", key.id))? else {
            return Ok(None);
        };
        let document = Document::parse(&text)?;
        ensure!(
            Key::from_document(&document)? == key,
            "document identity does not match bundle path"
        );
        Ok(Some(RevisionedDocument {
            key,
            revision: digest(text.as_bytes()),
            document,
        }))
    }

    fn check_revision(current: Option<&str>, expected: ExpectedRevision<'_>) -> Result<()> {
        let matches = match expected {
            ExpectedRevision::Absent => current.is_none(),
            ExpectedRevision::Digest(expected) => {
                current.is_some_and(|text| digest(text.as_bytes()) == expected)
            }
        };
        ensure!(
            matches,
            "knowledge revision conflict; reload before editing"
        );
        Ok(())
    }

    /// Success means file contents and directory entry have been synced. A
    /// failure after rename is ambiguous: reload and compare before retrying.
    pub fn put(
        &self,
        document: &Document,
        expected: ExpectedRevision<'_>,
    ) -> Result<RevisionedDocument> {
        let key = Key::from_document(document)?;
        let text = document.serialize()?;
        // Validate the serialized form with the same bounded parser used by reads.
        Document::parse(&text)?;
        let _lock = self.lock()?;
        self.recover_move_locked()?;
        self.check_unique(key)?;
        let parent = self.parent(key, true)?;
        let name = format!("{}.md", key.id);
        Self::check_revision(Self::read_at(&parent, &name)?.as_deref(), expected)?;
        Self::replace_at(&parent, &name, &text)?;
        Ok(RevisionedDocument {
            key,
            revision: digest(text.as_bytes()),
            document: document.clone(),
        })
    }

    fn replace_at(parent: &File, name: &str, text: &str) -> Result<()> {
        Self::replace_with_sync(parent, name, text, true)
    }

    /// Disposable caches need complete cross-process publication, not power-loss
    /// durability. Lost receipts may repeat eligible rules; SQL retains usage.
    /// Never use this for documents, policy, baselines or migration journals.
    fn replace_cache_at(parent: &File, name: &str, text: &str) -> Result<()> {
        Self::replace_with_sync(parent, name, text, false)
    }

    fn replace_with_sync(parent: &File, name: &str, text: &str, sync: bool) -> Result<()> {
        let temporary = format!(".{}.tmp", Uuid::new_v4());
        let result = (|| -> Result<()> {
            let mut file = child(
                &parent,
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            file.write_all(text.as_bytes())?;
            if sync {
                file.sync_all()?;
            }
            let old = CString::new(temporary.as_str())?;
            let new = CString::new(name)?;
            if unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    old.as_ptr(),
                    parent.as_raw_fd(),
                    new.as_ptr(),
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if sync {
                parent.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = unlink(&parent, &temporary);
        }
        result
    }

    pub(super) fn read_control(&self, name: &str) -> Result<Option<String>> {
        ensure!(
            !name.contains('/') && !name.contains('\\') && name != "..",
            "invalid control filename"
        );
        Self::read_at(&self.root, name)
    }

    pub(super) fn update_control<T>(
        &self,
        name: &str,
        update: impl FnOnce(Option<&str>) -> Result<(String, T)>,
    ) -> Result<T> {
        self.update_control_locked(name, update, self.lock()?, Self::replace_at)
    }

    pub(crate) fn remove_control(&self, name: &str, expected: &str) -> Result<()> {
        let _lease = self.lock()?;
        if let Some(current) = self.read_control(name)? {
            ensure!(current == expected, "control changed; refusing removal");
            unlink(&self.root, name)?;
        }
        self.root.sync_all()?;
        Ok(())
    }

    /// Optional state cannot hold up a hook indefinitely behind a paused process.
    /// Callers retain eligible knowledge and tolerate unavailable cache updates.
    pub(super) fn update_optional_control<T>(
        &self,
        name: &str,
        update: impl FnOnce(Option<&str>) -> Result<(String, T)>,
    ) -> Result<T> {
        self.update_control_locked(
            name,
            update,
            self.bounded_lock(".writer.lock")?,
            Self::replace_cache_at,
        )
    }

    /// Independent session receipts do not serialize corpus revalidation. Keep
    /// a shared legacy writer lease so older global-lock publishers still exclude
    /// us, and an exclusive receipt lease so one session cannot double-claim.
    pub(super) fn update_session_receipt<T>(
        &self,
        identity: &str,
        update: impl FnOnce(Option<&str>) -> Result<(String, T)>,
    ) -> Result<T> {
        ensure!(
            identity.len() == 64 && identity.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid session receipt identity"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let _legacy = self.bounded_lock_until(".writer.lock", false, deadline)?;
        let _receipt =
            self.bounded_lock_until(&format!(".session-{identity}.lock"), true, deadline)?;
        let name = format!("{identity}.json");
        let current = self.read_control(&name)?;
        let (text, result) = update(current.as_deref())?;
        ensure!(
            text.len() <= MAX_DOCUMENT_BYTES,
            "session receipt exceeds byte limit"
        );
        if current.as_deref() != Some(&text) {
            Self::replace_cache_at(&self.root, &name, &text)?;
        }
        Ok(result)
    }

    pub(super) fn bounded_lock(&self, name: &str) -> Result<File> {
        self.bounded_lock_until(
            name,
            true,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        )
    }

    fn bounded_lock_until(
        &self,
        name: &str,
        exclusive: bool,
        deadline: std::time::Instant,
    ) -> Result<File> {
        ensure!(
            !name.contains('/') && !name.contains('\\') && name != "..",
            "invalid lock filename"
        );
        let file = lock_file(&self.root, name)?;
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "invalid optional-state writer lease"
        );
        loop {
            let acquired = if exclusive {
                file.try_lock_exclusive()
            } else {
                FileExt::try_lock_shared(&file)
            };
            match acquired {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(file)
    }

    fn update_control_locked<T>(
        &self,
        name: &str,
        update: impl FnOnce(Option<&str>) -> Result<(String, T)>,
        _lock: File,
        publish: fn(&File, &str, &str) -> Result<()>,
    ) -> Result<T> {
        self.recover_move_locked()?;
        let current = self.read_control(name)?;
        let (text, result) = update(current.as_deref())?;
        ensure!(
            text.len() <= MAX_DOCUMENT_BYTES,
            "control document exceeds byte limit"
        );
        if current.as_deref() != Some(&text) {
            publish(&self.root, name, &text)?;
        }
        Ok(result)
    }

    pub fn delete(&self, key: Key, expected: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.recover_move_locked()?;
        self.check_unique(key)?;
        let parent = self.parent(key, false)?;
        let name = format!("{}.md", key.id);
        Self::check_revision(
            Self::read_at(&parent, &name)?.as_deref(),
            ExpectedRevision::Digest(expected),
        )?;
        unlink(&parent, &name)?;
        parent.sync_all()?;
        Ok(())
    }

    /// Scan fresh bytes. No cached approval or exclusive index content can make
    /// a deleted, edited or corrupted document eligible. One corrupt file does
    /// not suppress unrelated knowledge.
    fn inventory(&self) -> Inventory {
        let mut result = Inventory::default();
        let mut scopes = vec![None];
        match directory(&self.root, "repos", false) {
            Ok(repos) => match names(&repos) {
                Ok(names) => {
                    for name in names {
                        match Uuid::parse_str(&name) {
                            Ok(id) if id.to_string() == name => scopes.push(Some(id)),
                            _ => result
                                .diagnostics
                                .push(format!("repos/{name}: invalid portable repo UUID")),
                        }
                    }
                }
                Err(e) => {
                    result.incomplete = true;
                    result.diagnostics.push(format!("repos: {e}"));
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                result.incomplete = true;
                result.diagnostics.push(format!("repos: {e}"));
            }
        }
        for repo in scopes {
            for kind in [Kind::Note, Kind::Learning] {
                let key = Key {
                    repo,
                    kind,
                    id: Uuid::nil(),
                };
                let parent = match self.parent(key, false) {
                    Ok(parent) => parent,
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        continue;
                    }
                    Err(e) => {
                        result.incomplete = true;
                        result
                            .diagnostics
                            .push(format!("{:?}: {e}", key.relative_path().parent().unwrap()));
                        continue;
                    }
                };
                match names(&parent) {
                    Ok(names) => {
                        for name in names {
                            if name.starts_with('.') && name.ends_with(".tmp") {
                                continue;
                            }
                            let id = name
                                .strip_suffix(".md")
                                .and_then(|stem| Uuid::parse_str(stem).ok())
                                .filter(|id| format!("{id}.md") == name);
                            let Some(id) = id else {
                                result
                                    .diagnostics
                                    .push(format!("{name}: invalid document filename"));
                                continue;
                            };
                            let key = Key { id, ..key };
                            result.keys.push(key);
                        }
                    }
                    Err(e) => {
                        result.incomplete = true;
                        result.diagnostics.push(e.to_string());
                    }
                }
            }
        }
        result
    }
    pub fn snapshot(&self) -> Snapshot {
        if let Err(e) = self.recover_move() {
            return Snapshot {
                documents: Vec::new(),
                diagnostics: vec![format!("scope move recovery: {e}")],
            };
        }
        self.snapshot_under_lease()
    }

    // The caller already recovered pending moves and holds the writer lease.
    fn snapshot_under_lease(&self) -> Snapshot {
        let inventory = self.inventory();
        let mut result = Snapshot {
            documents: Vec::new(),
            diagnostics: inventory.diagnostics,
        };
        let mut counts = std::collections::HashMap::new();
        for key in &inventory.keys {
            *counts.entry(key.id).or_insert(0) += 1;
        }
        for key in inventory.keys {
            if counts[&key.id] != 1 {
                result.diagnostics.push(format!(
                    "{}: duplicate document UUID",
                    key.relative_path().display()
                ));
                continue;
            }
            match self.get_unchecked(key) {
                Ok(Some(doc)) => result.documents.push(doc),
                Ok(None) => {}
                Err(e) => result
                    .diagnostics
                    .push(format!("{}: {e}", key.relative_path().display())),
            }
        }
        result
    }

    /// Revalidate one injection batch against a single fresh UUID inventory.
    /// Individual file bytes are still read and hashed immediately; this is not
    /// a transaction snapshot across files or a cached membership assertion.
    pub fn revalidate_selected(&self, selected: &[RevisionedDocument]) -> Snapshot {
        if let Err(e) = self.recover_move() {
            return Snapshot {
                documents: Vec::new(),
                diagnostics: vec![format!("scope move recovery: {e}")],
            };
        }
        let inventory = self.inventory();
        let mut result = Snapshot {
            documents: Vec::new(),
            diagnostics: inventory.diagnostics,
        };
        if inventory.incomplete {
            return result;
        }
        let mut counts = std::collections::HashMap::new();
        for key in inventory.keys {
            *counts.entry(key.id).or_insert(0) += 1;
        }
        let mut seen = std::collections::HashSet::new();
        for previous in selected {
            if !seen.insert(previous.key.id) {
                continue;
            }
            if counts.get(&previous.key.id).copied().unwrap_or(0) != 1 {
                result.diagnostics.push(format!(
                    "{}: selected UUID missing or ambiguous",
                    previous.key.id
                ));
                continue;
            }
            match self.get_unchecked(previous.key) {
                Ok(Some(current)) if current.revision == previous.revision => {
                    result.documents.push(current)
                }
                Ok(_) => {}
                Err(e) => result.diagnostics.push(format!("{}: {e}", previous.key.id)),
            }
        }
        result
    }

    pub fn find(&self, id: Uuid) -> Result<Option<RevisionedDocument>> {
        self.recover_move()?;
        let inventory = self.inventory();
        ensure!(
            !inventory.incomplete,
            "cannot resolve UUID in an incomplete bundle inventory"
        );
        let candidates: Vec<_> = inventory.keys.into_iter().filter(|k| k.id == id).collect();
        ensure!(candidates.len() <= 1, "ambiguous document UUID");
        match candidates.first() {
            Some(key) => self.get_unchecked(*key),
            None => Ok(None),
        }
    }

    fn check_unique(&self, key: Key) -> Result<()> {
        let inventory = self.inventory();
        ensure!(
            !inventory.incomplete,
            "cannot mutate an incomplete bundle inventory"
        );
        ensure!(
            inventory.keys.iter().all(|k| k.id != key.id || *k == key),
            "document UUID already exists in another scope or type"
        );
        Ok(())
    }
}

#[cfg(test)]
mod session_receipt_tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn unrelated_sessions_do_not_wait_for_another_sessions_revalidation() {
        let temp = tempfile::tempdir().unwrap();
        let first = KnowledgeStore::open(&temp.path().join("sessions"), true).unwrap();
        let second = KnowledgeStore::open(&temp.path().join("sessions"), false).unwrap();
        let (entered, ready) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                first.update_session_receipt(&"1".repeat(64), |_| {
                    entered.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(("first".into(), ()))
                })
            });
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let result =
                second.update_session_receipt(&"2".repeat(64), |_| Ok(("second".into(), ())));
            release.send(()).unwrap();
            worker.join().unwrap().unwrap();
            result.unwrap();
        });
    }

    #[test]
    fn same_session_updates_read_the_prior_committed_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let first = KnowledgeStore::open(&temp.path().join("sessions"), true).unwrap();
        let second = KnowledgeStore::open(&temp.path().join("sessions"), false).unwrap();
        let (entered, ready) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                first.update_session_receipt(&"1".repeat(64), |_| {
                    entered.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(("first".into(), ()))
                })
            });
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let next = scope.spawn(move || {
                second.update_session_receipt(&"1".repeat(64), |prior| {
                    assert_eq!(prior, Some("first"));
                    Ok(("second".into(), ()))
                })
            });
            release.send(()).unwrap();
            worker.join().unwrap().unwrap();
            next.join().unwrap().unwrap();
        });
    }

    #[test]
    fn session_updates_respect_legacy_global_writer_leases() {
        let temp = tempfile::tempdir().unwrap();
        let first = KnowledgeStore::open(&temp.path().join("sessions"), true).unwrap();
        let second = KnowledgeStore::open(&temp.path().join("sessions"), false).unwrap();
        let lease = first.bounded_lock(".writer.lock").unwrap();
        let (entered, ready) = mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                second.update_session_receipt(&"1".repeat(64), |_| {
                    entered.send(()).unwrap();
                    Ok(("receipt".into(), ()))
                })
            });
            assert!(ready.recv_timeout(Duration::from_millis(50)).is_err());
            drop(lease);
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            worker.join().unwrap().unwrap();
        });
    }
}
