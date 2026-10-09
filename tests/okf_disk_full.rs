//! Opt-in real ENOSPC qualification, confined to an owned small filesystem.
#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::PathBuf,
    process::Command,
};
use ygg::knowledge::{
    document::Document,
    store::{ExpectedRevision, KnowledgeStore},
};

struct FullFilesystem {
    root: PathBuf,
    mount: PathBuf,
}

fn checked(command: &mut Command) {
    let output = command.output().expect("fixture command must start");
    assert!(
        output.status.success(),
        "fixture command failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

impl FullFilesystem {
    fn new() -> Self {
        // Retain paths on a failed detach; TempDir must never recursively remove
        // a still-mounted volume after a cleanup error.
        let root = tempfile::Builder::new()
            .prefix("ygg-okf-disk-full-")
            .tempdir()
            .unwrap()
            .keep()
            .canonicalize()
            .unwrap();
        let mount = root.join("mount");
        fs::create_dir(&mount).unwrap();
        #[cfg(target_os = "macos")]
        checked(
            Command::new("hdiutil")
                .args([
                    "create",
                    "-size",
                    "128m",
                    "-fs",
                    "APFS",
                    "-volname",
                    "YggDiskFullFixture",
                    "-type",
                    "UDIF",
                ])
                .arg(root.join("fixture.dmg")),
        );
        let fixture = Self { root, mount };
        eprintln!("disk-full fixture: {}", fixture.root.display());
        #[cfg(target_os = "macos")]
        checked(
            Command::new("hdiutil")
                .arg("attach")
                .arg(fixture.root.join("fixture.dmg"))
                .args(["-nobrowse", "-owners", "on", "-mountpoint"])
                .arg(&fixture.mount),
        );
        #[cfg(target_os = "linux")]
        checked(
            Command::new("sudo")
                .args(["-n", "--", "mount", "-t", "tmpfs", "-o"])
                .arg(format!(
                    "size=128m,mode=0700,uid={},gid={},nodev,nosuid,noexec",
                    unsafe { libc::geteuid() },
                    unsafe { libc::getegid() }
                ))
                .arg("tmpfs")
                .arg(&fixture.mount),
        );
        assert_ne!(
            fs::metadata(&fixture.mount).unwrap().dev(),
            fs::metadata(&fixture.root).unwrap().dev(),
            "refuse to fill the host filesystem"
        );
        let name = CString::new(fixture.mount.as_os_str().as_bytes()).unwrap();
        let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        assert_eq!(
            unsafe { libc::statvfs(name.as_ptr(), info.as_mut_ptr()) },
            0
        );
        let info = unsafe { info.assume_init() };
        let bytes = (info.f_blocks as u128) * (info.f_frsize as u128);
        assert!(
            (4 * 1024 * 1024..=256 * 1024 * 1024).contains(&bytes),
            "unexpected fixture capacity: {bytes}"
        );
        fixture
    }

    fn fill(&self) -> PathBuf {
        let path = self.mount.join("filler");
        let mut output = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .unwrap();
        let block: Vec<u8> = (0..65536).map(|i| ((i * 31 + 17) % 251) as u8).collect();
        let mut written = output.metadata().unwrap().len();
        loop {
            match output.write_all(&block) {
                Ok(()) => {
                    written += block.len() as u64;
                    assert!(
                        written <= 256 * 1024 * 1024,
                        "fixture did not reach its capacity"
                    );
                }
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                    break;
                }
            }
        }
        if let Err(error) = output.sync_all() {
            assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        }
        eprintln!("real ENOSPC after {written} bytes");
        path
    }
}

impl Drop for FullFilesystem {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        let output = Command::new("hdiutil")
            .arg("detach")
            .arg(&self.mount)
            .output();
        #[cfg(target_os = "linux")]
        let output = Command::new("sudo")
            .args(["-n", "--", "umount"])
            .arg(&self.mount)
            .output();
        match output {
            Ok(output) if output.status.success() => {
                fs::remove_dir_all(&self.root).expect("remove detached fixture");
            }
            other => {
                eprintln!(
                    "fixture detach failed; retained {}: {other:?}",
                    self.root.display()
                );
                assert!(std::thread::panicking(), "fixture cleanup failed");
            }
        }
    }
}

fn no_space(error: &anyhow::Error) {
    assert!(
        error.chain().any(|cause| cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.raw_os_error() == Some(libc::ENOSPC))),
        "expected actual ENOSPC, got {error:#}"
    );
}

#[test]
#[ignore = "mounts owned 128 MiB APFS image (macOS) or tmpfs (Linux; sudo -n required)"]
fn actual_disk_full_preserves_acknowledged_documents_and_allows_retry() {
    let filesystem = FullFilesystem::new();
    let root = filesystem.mount.join("knowledge");
    let store = KnowledgeStore::open(&root, true).unwrap();
    let mut acknowledged = store
        .put(
            &Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap(),
            ExpectedRevision::Absent,
        )
        .unwrap();
    let filler = filesystem.fill();
    let mut edited = acknowledged.document.clone();
    let mut update_failed = false;
    // ENOSPC from the filler does not promise that a later operation will also
    // fail: the filesystem may make space available between operations. Any
    // successful publication is acknowledged state, never a failed-write case.
    for attempt in 0..8 {
        if attempt > 0 {
            filesystem.fill();
        }
        edited.body = format!("attempt {attempt}\n")
            + &"full filesystem must not replace acknowledged bytes\n".repeat(10_000);
        match store.put(&edited, ExpectedRevision::Digest(&acknowledged.revision)) {
            Ok(saved) => {
                eprintln!("update succeeded after filler ENOSPC; refill attempt {attempt}");
                acknowledged = saved;
            }
            Err(error) => {
                no_space(&error);
                update_failed = true;
                break;
            }
        }
    }
    assert!(
        update_failed,
        "did not observe an actual ENOSPC update failure"
    );
    let current = store.get(acknowledged.key).unwrap().unwrap();
    assert_eq!(current.revision, acknowledged.revision);
    assert_eq!(current.document, acknowledged.document);

    let mut documents = vec![acknowledged.clone()];
    let mut create_failed = false;
    for attempt in 0..8 {
        filesystem.fill();
        let mut fresh = edited.clone();
        let mut profile = fresh.profile().unwrap().unwrap();
        profile.id = uuid::Uuid::new_v4();
        fresh.set_profile(&profile).unwrap();
        match store.put(&fresh, ExpectedRevision::Absent) {
            Ok(saved) => {
                eprintln!("create succeeded after filler ENOSPC; refill attempt {attempt}");
                documents.push(saved);
            }
            Err(error) => {
                no_space(&error);
                create_failed = true;
                break;
            }
        }
    }
    assert!(
        create_failed,
        "did not observe an actual ENOSPC create failure"
    );
    let snapshot = store.snapshot();
    assert!(snapshot.diagnostics.is_empty());
    assert_eq!(snapshot.documents.len(), documents.len());
    for saved in &documents {
        let current = store.get(saved.key).unwrap().unwrap();
        assert_eq!(current.revision, saved.revision);
        assert_eq!(current.document, saved.document);
    }
    let parent = root
        .join(acknowledged.key.relative_path())
        .parent()
        .unwrap()
        .to_owned();
    assert_eq!(
        fs::read_dir(parent).unwrap().count(),
        documents.len(),
        "failed writes left staging files"
    );

    fs::remove_file(filler).unwrap();
    File::open(&filesystem.mount).unwrap().sync_all().unwrap();
    let replacement = store
        .put(&edited, ExpectedRevision::Digest(&acknowledged.revision))
        .unwrap();
    drop(store);
    let reopened = KnowledgeStore::open(&root, false)
        .unwrap()
        .get(acknowledged.key)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.revision, replacement.revision);
    assert_eq!(reopened.document, edited);
    assert_ne!(reopened.revision, acknowledged.revision);
}
