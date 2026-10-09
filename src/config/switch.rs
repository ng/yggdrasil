//! Durable, conditional publication of one complete deployment configuration.
//! Filesystem leases coordinate Yggdrasil writers; external editors must quiesce.
use crate::knowledge::document::digest;
use anyhow::{Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};
use uuid::Uuid;

const LIMIT: u64 = 1024 * 1024;
fn open(parent: &File, name: &str, flags: i32) -> Result<File> {
    let name = CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::geteuid() },
        "configuration entry must be an owned regular non-hardlinked file"
    );
    Ok(file)
}
fn read(parent: &File, name: &str) -> Result<Option<Vec<u8>>> {
    let file = match open(parent, name, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "configuration entry exceeds size limit"
    );
    Ok(Some(bytes))
}
fn create(parent: &File, name: &str, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "configuration entry exceeds size limit"
    );
    let mut file = open(parent, name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn directory(parent: &File, name: &str, new: bool) -> Result<File> {
    let c = CString::new(name)?;
    if new {
        ensure!(
            unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o700) } == 0,
            "cannot create switch journal: {}",
            std::io::Error::last_os_error()
        );
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    private(&file)?;
    Ok(file)
}
fn private(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "configuration and journal directories must be private and owned"
    );
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32,
    operation: Uuid,
    binding: String,
    previous: Option<String>,
    next: String,
}
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub operation: Uuid,
    pub previous_sha256: Option<String>,
    pub current_sha256: String,
    pub already_applied: bool,
}

pub struct Publication {
    root: File,
    journal: File,
    _lock: File,
    intent: Intent,
    previous: Option<Vec<u8>>,
    next: Vec<u8>,
    resumed: bool,
}
impl Publication {
    /// Capture the current configuration before validation; retain exact previous
    /// bytes and proposed bytes in a private immutable journal for recovery.
    pub fn begin(root: &Path, next: &[u8], binding: &str, resume: Option<Uuid>) -> Result<Self> {
        ensure!(
            next.len() as u64 <= LIMIT,
            "target configuration exceeds size limit"
        );
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)?;
        private(&root)?;
        let lock = open(
            &root,
            ".deployment-switch.lock",
            libc::O_RDWR | libc::O_CREAT,
        )?;
        lock.try_lock_exclusive().map_err(|_| {
            anyhow::anyhow!("another deployment switch holds the configuration lease")
        })?;
        let operation = resume.unwrap_or_else(Uuid::new_v4);
        let journal = directory(
            &root,
            &format!("deployment-switch-{operation}"),
            resume.is_none(),
        )?;
        let (intent, previous) = if resume.is_some() {
            let encoded = read(&journal, "intent.json")?
                .ok_or_else(|| anyhow::anyhow!("switch journal lacks a durable intent"))?;
            let intent: Intent = serde_json::from_slice(&encoded)?;
            ensure!(
                intent.version == 1
                    && intent.operation == operation
                    && intent.binding == binding
                    && intent.next == digest(next),
                "switch journal does not match this operation"
            );
            let saved = read(&journal, "next.toml")?
                .ok_or_else(|| anyhow::anyhow!("switch journal lacks proposed configuration"))?;
            let previous = read(&journal, "previous.toml")?;
            ensure!(
                saved == next && previous.as_ref().map(|b| digest(b)) == intent.previous,
                "switch journal content changed"
            );
            (intent, previous)
        } else {
            let previous = read(&root, "config.toml")?;
            let intent = Intent {
                version: 1,
                operation,
                binding: binding.into(),
                previous: previous.as_ref().map(|b| digest(b)),
                next: digest(next),
            };
            if let Some(previous) = &previous {
                create(&journal, "previous.toml", previous)?;
            }
            create(&journal, "next.toml", next)?;
            create(&journal, "intent.json", &serde_json::to_vec(&intent)?)?;
            journal.sync_all()?;
            root.sync_all()?;
            (intent, previous)
        };
        Ok(Self {
            root,
            journal,
            _lock: lock,
            intent,
            previous,
            next: next.to_vec(),
            resumed: resume.is_some(),
        })
    }
    pub fn operation(&self) -> Uuid {
        self.intent.operation
    }
    pub fn already_applied(&self) -> Result<bool> {
        let current = read(&self.root, "config.toml")?;
        // A no-op proposal interrupted before validation must still validate.
        if self.resumed
            && current.as_deref() == Some(&self.next)
            && (self.previous.as_deref() != Some(&self.next)
                || read(&self.journal, "committed.json")?.is_some())
        {
            let validated = read(&self.journal, "validated.json")?;
            ensure!(
                validated.as_deref() == Some(serde_json::to_vec(&self.intent)?.as_slice()),
                "configuration matches proposal without durable validation evidence; resolve explicitly"
            );
            self.root.sync_all()?;
            self.receipt()?;
            return Ok(true);
        }
        ensure!(
            current == self.previous,
            "configuration changed since switch intent; preserve the journal and resolve explicitly"
        );
        Ok(false)
    }
    fn receipt(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.intent)?;
        match read(&self.journal, "committed.json")? {
            Some(actual) => ensure!(actual == bytes, "switch completion receipt changed"),
            None => create(&self.journal, "committed.json", &bytes)?,
        }
        self.journal.sync_all()?;
        Ok(())
    }
    pub fn outcome(&self, already_applied: bool) -> Outcome {
        Outcome {
            operation: self.intent.operation,
            previous_sha256: self.intent.previous.clone(),
            current_sha256: self.intent.next.clone(),
            already_applied,
        }
    }
    pub fn commit(&self) -> Result<Outcome> {
        self.commit_at(&|_| {})
    }
    fn commit_at(&self, checkpoint: &dyn Fn(&str)) -> Result<Outcome> {
        let validated = serde_json::to_vec(&self.intent)?;
        match read(&self.journal, "validated.json")? {
            Some(actual) => ensure!(actual == validated, "switch validation evidence changed"),
            None => create(&self.journal, "validated.json", &validated)?,
        }
        self.journal.sync_all()?;
        let name = format!(".config-{}.tmp", Uuid::new_v4());
        create(&self.root, &name, &self.next)?;
        checkpoint("prepared");
        ensure!(
            read(&self.root, "config.toml")? == self.previous,
            "configuration changed during validation; switch refused"
        );
        let from = CString::new(name)?;
        let to = c"config.toml";
        ensure!(
            unsafe {
                libc::renameat(
                    self.root.as_raw_fd(),
                    from.as_ptr(),
                    self.root.as_raw_fd(),
                    to.as_ptr(),
                )
            } == 0,
            "cannot publish configuration: {}",
            std::io::Error::last_os_error()
        );
        self.root.sync_all()?;
        checkpoint("published");
        self.receipt()?;
        checkpoint("committed");
        Ok(self.outcome(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_intent_cannot_certify_an_independently_matching_configuration() {
        use std::os::unix::fs::PermissionsExt;
        for old in [b"old".as_slice(), b"new".as_slice()] {
            let temp = tempfile::tempdir().unwrap();
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(temp.path().join("config.toml"), old).unwrap();
            let publication = Publication::begin(temp.path(), b"new", "binding", None).unwrap();
            let id = publication.operation();
            drop(publication);
            std::fs::write(temp.path().join("config.toml"), b"new").unwrap();
            let publication = Publication::begin(temp.path(), b"new", "binding", Some(id)).unwrap();
            if old == b"new" {
                assert!(!publication.already_applied().unwrap());
            } else {
                assert!(publication.already_applied().is_err());
            }
        }
    }
    #[test]
    fn crash_helper() {
        let Ok(path) = std::env::var("YGG_SWITCH_CRASH_ROOT") else {
            return;
        };
        let point = std::env::var("YGG_SWITCH_CRASH_POINT").unwrap();
        let operation =
            Uuid::parse_str(&std::env::var("YGG_SWITCH_CRASH_OPERATION").unwrap()).unwrap();
        let publication =
            Publication::begin(Path::new(&path), b"new", "binding", Some(operation)).unwrap();
        publication
            .commit_at(&|here| {
                if here == point {
                    std::fs::write(Path::new(&path).join("ready"), here).unwrap();
                    loop {
                        std::thread::park();
                    }
                }
            })
            .unwrap();
    }
    #[test]
    fn killed_publication_resumes_without_overwriting_edits() {
        for point in ["prepared", "published", "committed"] {
            let temp = tempfile::tempdir().unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(temp.path().join("config.toml"), b"old").unwrap();
            let publication = Publication::begin(temp.path(), b"new", "binding", None).unwrap();
            let id = publication.operation();
            drop(publication);
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "config::switch::tests::crash_helper"])
                .env("YGG_SWITCH_CRASH_ROOT", temp.path())
                .env("YGG_SWITCH_CRASH_POINT", point)
                .env("YGG_SWITCH_CRASH_OPERATION", id.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !temp.path().join("ready").exists() {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("switch checkpoint timeout");
                }
                assert!(child.try_wait().unwrap().is_none());
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            child.kill().unwrap();
            child.wait().unwrap();
            let publication = Publication::begin(temp.path(), b"new", "binding", Some(id)).unwrap();
            assert_eq!(publication.already_applied().unwrap(), point != "prepared");
            if point == "prepared" {
                publication.commit().unwrap();
            }
            assert_eq!(
                std::fs::read(temp.path().join("config.toml")).unwrap(),
                b"new"
            );
            drop(publication);
            std::fs::write(temp.path().join("config.toml"), b"independent edit").unwrap();
            let publication = Publication::begin(temp.path(), b"new", "binding", Some(id)).unwrap();
            assert!(publication.already_applied().is_err());
            assert_eq!(
                std::fs::read(temp.path().join("config.toml")).unwrap(),
                b"independent edit"
            );
        }
    }
}
