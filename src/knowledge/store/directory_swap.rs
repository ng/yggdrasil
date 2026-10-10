//! Recoverable filesystem half of fleet finalization. This is not activation
//! authority: callers must retain this plan before use, hold the host selection
//! lease, verify database authority and content backups, and stop other editors.
use super::{KnowledgeStore, child};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::File,
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

type Identity = (u64, u64);
fn identity(file: &File) -> Result<Identity> {
    let m = file.metadata()?;
    ensure!(m.is_dir(), "swap entry must be a directory");
    Ok((m.dev(), m.ino()))
}
fn parent_directory(path: &Path) -> Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
        "swap parent must be owned and not writable by other users"
    );
    Ok(file)
}
// Device IDs alone cannot distinguish Linux bind mounts. Query the mount
// containing each open descriptor, so pathname aliases cannot hide a boundary.
#[cfg(target_os = "linux")]
fn mount_key(file: &File) -> Result<Vec<u8>> {
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            stat.as_mut_ptr(),
        )
    };
    ensure!(
        result == 0,
        "cannot inspect swap mount: {}",
        std::io::Error::last_os_error()
    );
    let stat = unsafe { stat.assume_init() };
    ensure!(
        stat.stx_mask & libc::STATX_MNT_ID != 0,
        "kernel does not expose swap mount identity"
    );
    Ok(stat.stx_mnt_id.to_le_bytes().to_vec())
}
#[cfg(target_os = "macos")]
fn mount_key(file: &File) -> Result<Vec<u8>> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    let result = unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) };
    ensure!(
        result == 0,
        "cannot inspect swap mount: {}",
        std::io::Error::last_os_error()
    );
    let stat = unsafe { stat.assume_init() };
    let end = stat
        .f_mntonname
        .iter()
        .position(|byte| *byte == 0)
        .context("invalid swap mount name")?;
    ensure!(end > 0, "empty swap mount name");
    Ok(stat.f_mntonname[..end]
        .iter()
        .map(|byte| *byte as u8)
        .collect())
}
fn same_mount(parent: &File, child: &File) -> Result<()> {
    ensure!(
        mount_key(parent)? == mount_key(child)?,
        "swap directory crosses a mount boundary"
    );
    Ok(())
}
fn entry(parent: &File, name: &str) -> Result<Option<Identity>> {
    match child(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
        Ok(file) => {
            same_mount(parent, &file)?;
            Ok(Some(identity(&file)?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn name(path: &Path) -> Result<&str> {
    let n = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("swap path needs a UTF-8 name")?;
    ensure!(
        !matches!(n, "." | "..") && !n.is_empty(),
        "invalid swap name"
    );
    Ok(n)
}
fn rename(from: &File, source: &str, to: &File, target: &str) -> Result<()> {
    let source = CString::new(source)?;
    let target = CString::new(target)?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    ensure!(
        result == 0,
        "exclusive directory rename failed: {}",
        std::io::Error::last_os_error()
    );
    // Persist both directory entries. Failure is ambiguous and must be resumed
    // by inspecting identities, never by assuming the rename did not happen.
    from.sync_all()?;
    to.sync_all()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectorySwapPlan {
    version: u32,
    corpus: PathBuf,
    staging: PathBuf,
    parent: Identity,
    stage: Identity,
    original: Identity,
    candidate: Identity,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorySwapState {
    Prepared,
    OriginalRetained,
    CandidateInstalled,
}
struct Roots {
    parent: File,
    stage: KnowledgeStore,
}
impl DirectorySwapPlan {
    /// Capture an existing private sibling layout without moving anything.
    /// The parent must be owned and not writable by others. Capture proves
    /// identity, same-filesystem, and same-mount layout. It does not prove all
    /// filesystem rename permissions or grant activation authority.
    pub fn capture(corpus: &Path, staging: &Path) -> Result<Self> {
        ensure!(
            corpus.is_absolute() && staging.is_absolute(),
            "swap paths must be absolute"
        );
        ensure!(
            corpus.canonicalize()? == corpus && staging.canonicalize()? == staging,
            "swap paths must be canonical, without symlink aliases"
        );
        ensure!(
            corpus != staging && corpus.parent() == staging.parent(),
            "swap roots must be distinct siblings"
        );
        let parent = parent_directory(corpus.parent().context("corpus parent missing")?)?;
        let stage = KnowledgeStore::open(staging, false)?;
        let original = KnowledgeStore::open(corpus, false)?;
        let candidate = KnowledgeStore::open(&staging.join("candidate"), false)?;
        let plan = Self {
            version: 1,
            corpus: corpus.to_owned(),
            staging: staging.to_owned(),
            parent: identity(&parent)?,
            stage: identity(&stage.root)?,
            original: identity(&original.root)?,
            candidate: identity(&candidate.root)?,
        };
        ensure!(
            plan.original != plan.candidate
                && [plan.stage.0, plan.original.0, plan.candidate.0]
                    .iter()
                    .all(|dev| *dev == plan.parent.0),
            "swap directories must be distinct and on one filesystem"
        );
        ensure!(
            plan.inspect()? == DirectorySwapState::Prepared,
            "swap must start with original and candidate present"
        );
        Ok(plan)
    }
    fn roots(&self) -> Result<Roots> {
        ensure!(
            self.version == 1
                && self.corpus.is_absolute()
                && self.staging.is_absolute()
                && self.corpus != self.staging
                && self.corpus.parent() == self.staging.parent(),
            "invalid retained swap plan"
        );
        let parent_path = self.corpus.parent().context("corpus parent missing")?;
        ensure!(
            parent_path.canonicalize()? == parent_path
                && self.staging.canonicalize()? == self.staging,
            "swap parent path changed"
        );
        let parent = parent_directory(parent_path)?;
        let stage = KnowledgeStore::open(&self.staging, false)?;
        ensure!(
            identity(&parent)? == self.parent
                && identity(&stage.root)? == self.stage
                && entry(&parent, name(&self.staging)?)? == Some(self.stage),
            "swap parent or staging identity changed"
        );
        Ok(Roots { parent, stage })
    }
    fn state(&self, roots: &Roots) -> Result<DirectorySwapState> {
        let current_parent = parent_directory(self.corpus.parent().unwrap())?;
        ensure!(
            identity(&current_parent)? == identity(&roots.parent)?,
            "swap parent path changed"
        );
        roots.stage.verify_root_path(&self.staging)?;
        let selected = entry(&roots.parent, name(&self.corpus)?)?;
        let candidate = entry(&roots.stage.root, "candidate")?;
        let original = entry(&roots.stage.root, "original")?;
        match (selected, candidate, original) {
            (Some(a), Some(b), None) if a == self.original && b == self.candidate => {
                Ok(DirectorySwapState::Prepared)
            }
            (None, Some(b), Some(a)) if a == self.original && b == self.candidate => {
                Ok(DirectorySwapState::OriginalRetained)
            }
            (Some(b), None, Some(a)) if a == self.original && b == self.candidate => {
                Ok(DirectorySwapState::CandidateInstalled)
            }
            _ => bail!("swap layout changed independently; retain all directories for recovery"),
        }
    }
    pub fn inspect(&self) -> Result<DirectorySwapState> {
        self.state(&self.roots()?)
    }
    /// Move at most one directory using no-replace semantics, then report the
    /// observed state. Retry an ambiguous outcome using the same retained plan.
    /// Installed candidates are never restored from frozen readiness data here.
    pub fn advance(&self) -> Result<DirectorySwapState> {
        let roots = self.roots()?;
        match self.state(&roots)? {
            DirectorySwapState::Prepared => rename(
                &roots.parent,
                name(&self.corpus)?,
                &roots.stage.root,
                "original",
            )?,
            DirectorySwapState::OriginalRetained => rename(
                &roots.stage.root,
                "candidate",
                &roots.parent,
                name(&self.corpus)?,
            )?,
            DirectorySwapState::CandidateInstalled => {}
        }
        // A previous process may have died after rename but before fsync.
        // Even an already-installed retry must make observed entries durable.
        roots.parent.sync_all()?;
        roots.stage.root.sync_all()?;
        self.state(&roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, DirectorySwapPlan) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        KnowledgeStore::open(&root, true).unwrap();
        let root = root.canonicalize().unwrap();
        let corpus = root.join("corpus");
        let staging = root.join("staging");
        KnowledgeStore::open(&corpus, true).unwrap();
        KnowledgeStore::open(&staging, true).unwrap();
        KnowledgeStore::open(&staging.join("candidate"), true).unwrap();
        std::fs::write(corpus.join("original.txt"), "old").unwrap();
        std::fs::write(staging.join("candidate/new.txt"), "new").unwrap();
        let plan = DirectorySwapPlan::capture(&corpus, &staging).unwrap();
        (temp, plan)
    }
    #[test]
    fn resumes_each_durable_boundary_and_preserves_later_writes() {
        let (_temp, plan) = fixture();
        let saved = serde_json::to_vec(&plan).unwrap();
        assert_eq!(
            plan.advance().unwrap(),
            DirectorySwapState::OriginalRetained
        );
        assert!(!plan.corpus.exists());
        let restarted: DirectorySwapPlan = serde_json::from_slice(&saved).unwrap();
        assert_eq!(
            restarted.inspect().unwrap(),
            DirectorySwapState::OriginalRetained
        );
        assert_eq!(
            restarted.advance().unwrap(),
            DirectorySwapState::CandidateInstalled
        );
        std::fs::write(plan.corpus.join("new.txt"), "later write").unwrap();
        let restarted: DirectorySwapPlan = serde_json::from_slice(&saved).unwrap();
        assert_eq!(
            restarted.advance().unwrap(),
            DirectorySwapState::CandidateInstalled
        );
        assert_eq!(
            std::fs::read_to_string(plan.corpus.join("new.txt")).unwrap(),
            "later write"
        );
        assert_eq!(
            std::fs::read_to_string(plan.staging.join("original/original.txt")).unwrap(),
            "old"
        );
    }
    #[test]
    fn refuses_independent_destinations_and_changed_sources() {
        for step in 0..3 {
            let (_temp, plan) = fixture();
            match step {
                0 => {
                    std::fs::create_dir(plan.staging.join("original")).unwrap();
                }
                1 => {
                    plan.advance().unwrap();
                    std::fs::create_dir(&plan.corpus).unwrap();
                }
                _ => {
                    std::fs::rename(
                        plan.staging.join("candidate"),
                        plan.staging.join("independent"),
                    )
                    .unwrap();
                    std::fs::create_dir(plan.staging.join("candidate")).unwrap();
                }
            }
            assert!(plan.advance().is_err());
            assert!(plan.staging.join("candidate").exists());
        }
    }
    #[test]
    #[ignore = "subprocess helper for killed directory swap"]
    fn swap_worker() {
        let Some(path) = std::env::var_os("YGG_TEST_SWAP_PLAN") else {
            return;
        };
        let path = PathBuf::from(path);
        let plan: DirectorySwapPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let count: usize = std::env::var("YGG_TEST_SWAP_STEPS")
            .unwrap()
            .parse()
            .unwrap();
        for _ in 0..count {
            plan.advance().unwrap();
        }
        std::fs::write(path.with_extension("ready"), "ready").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    #[test]
    fn killed_process_resumes_after_either_rename() {
        for count in [1, 2] {
            let (temp, plan) = fixture();
            let path = temp.path().join("plan.json");
            std::fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
            let mut worker = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "knowledge::store::directory_swap::tests::swap_worker",
                ])
                .env("YGG_TEST_SWAP_PLAN", &path)
                .env("YGG_TEST_SWAP_STEPS", count.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !path.with_extension("ready").exists() {
                if worker.try_wait().unwrap().is_some() || std::time::Instant::now() >= deadline {
                    let _ = worker.kill();
                    let _ = worker.wait();
                    panic!("swap worker did not reach boundary {count}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            worker.kill().unwrap();
            worker.wait().unwrap();
            let restarted: DirectorySwapPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                restarted.advance().unwrap(),
                DirectorySwapState::CandidateInstalled
            );
            assert_eq!(
                std::fs::read_to_string(plan.corpus.join("new.txt")).unwrap(),
                "new"
            );
            assert_eq!(
                std::fs::read_to_string(plan.staging.join("original/original.txt")).unwrap(),
                "old"
            );
        }
    }

    #[test]
    fn parent_may_be_readable_but_never_writable_by_others() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, plan) = fixture();
        let parent = plan.corpus.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(DirectorySwapPlan::capture(&plan.corpus, &plan.staging).is_ok());
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(DirectorySwapPlan::capture(&plan.corpus, &plan.staging).is_err());
        assert!(plan.advance().is_err());
        assert!(plan.corpus.join("original.txt").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_mount_identity_rejects_proc_boundary() {
        let root = File::open("/").unwrap();
        let proc = File::open("/proc").unwrap();
        assert_ne!(mount_key(&root).unwrap(), mount_key(&proc).unwrap());
        assert!(same_mount(&root, &proc).is_err());
    }

    #[test]
    fn capture_never_creates_a_missing_candidate() {
        let (_temp, plan) = fixture();
        std::fs::rename(plan.staging.join("candidate"), plan.staging.join("saved")).unwrap();
        assert!(DirectorySwapPlan::capture(&plan.corpus, &plan.staging).is_err());
        assert!(!plan.staging.join("candidate").exists());
        assert!(plan.corpus.join("original.txt").exists());
    }

    #[test]
    fn no_replace_syscall_rejects_raced_destination() {
        let (_temp, plan) = fixture();
        let roots = plan.roots().unwrap();
        assert_eq!(plan.state(&roots).unwrap(), DirectorySwapState::Prepared);
        std::fs::create_dir(plan.staging.join("original")).unwrap();
        assert!(
            rename(
                &roots.parent,
                name(&plan.corpus).unwrap(),
                &roots.stage.root,
                "original"
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(plan.corpus.join("original.txt")).unwrap(),
            "old"
        );
    }
    #[test]
    fn rejects_symlink_and_replaced_parent_without_moving_data() {
        let (_temp, plan) = fixture();
        let detached = plan.staging.with_file_name("detached");
        std::fs::rename(&plan.staging, &detached).unwrap();
        std::os::unix::fs::symlink(&detached, &plan.staging).unwrap();
        assert!(plan.advance().is_err());
        std::fs::remove_file(&plan.staging).unwrap();
        KnowledgeStore::open(&plan.staging, true).unwrap();
        assert!(plan.advance().is_err());
        assert!(detached.join("candidate/new.txt").exists());
        assert!(plan.corpus.join("original.txt").exists());
    }
}
