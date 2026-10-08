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
use uuid::Uuid;

use super::document::{Document, MAX_DOCUMENT_BYTES, Scope, digest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Note,
    Learning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            result.push(name);
        }
    }
    result.sort();
    Ok(result)
}

impl KnowledgeStore {
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

    fn lock(&self) -> Result<File> {
        let file = child(
            &self.root,
            ".writer.lock",
            libc::O_RDWR | libc::O_CREAT,
            0o600,
        )?;
        ensure!(
            file.metadata()?.is_file(),
            "knowledge lock is not a regular file"
        );
        file.lock_exclusive()?;
        Ok(file) // Drop releases the OS lock, including after a process crash.
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
        file.take((MAX_DOCUMENT_BYTES + 1) as u64)
            .read_to_string(&mut text)?;
        ensure!(
            text.len() <= MAX_DOCUMENT_BYTES,
            "knowledge document exceeds byte limit"
        );
        Ok(Some(text))
    }

    pub fn get(&self, key: Key) -> Result<Option<RevisionedDocument>> {
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
        let temporary = format!(".{}.tmp", Uuid::new_v4());
        let result = (|| -> Result<()> {
            let mut file = child(
                &parent,
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
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
            parent.sync_all()?;
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
        let _lock = self.lock()?;
        let current = self.read_control(name)?;
        let (text, result) = update(current.as_deref())?;
        ensure!(
            text.len() <= MAX_DOCUMENT_BYTES,
            "control document exceeds byte limit"
        );
        if current.as_deref() != Some(&text) {
            Self::replace_at(&self.root, name, &text)?;
        }
        Ok(result)
    }

    pub fn delete(&self, key: Key, expected: &str) -> Result<()> {
        let _lock = self.lock()?;
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
            match self.get(key) {
                Ok(Some(doc)) => result.documents.push(doc),
                Ok(None) => {}
                Err(e) => result
                    .diagnostics
                    .push(format!("{}: {e}", key.relative_path().display())),
            }
        }
        result
    }

    pub fn find(&self, id: Uuid) -> Result<Option<RevisionedDocument>> {
        let inventory = self.inventory();
        ensure!(
            !inventory.incomplete,
            "cannot resolve UUID in an incomplete bundle inventory"
        );
        let candidates: Vec<_> = inventory.keys.into_iter().filter(|k| k.id == id).collect();
        ensure!(candidates.len() <= 1, "ambiguous document UUID");
        match candidates.first() {
            Some(key) => self.get(*key),
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
