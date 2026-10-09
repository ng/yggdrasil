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
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let block: Vec<u8> = (0..65536).map(|i| ((i * 31 + 17) % 251) as u8).collect();
        let mut written = 0;
        loop {
            match output.write_all(&block) {
                Ok(()) => {
                    written += block.len();
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
    let original = store
        .put(
            &Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap(),
            ExpectedRevision::Absent,
        )
        .unwrap();
    let filler = filesystem.fill();
    let mut edited = original.document.clone();
    edited.body = "full filesystem must not replace acknowledged bytes\n".repeat(10_000);
    no_space(
        &store
            .put(&edited, ExpectedRevision::Digest(&original.revision))
            .unwrap_err(),
    );
    let current = store.get(original.key).unwrap().unwrap();
    assert_eq!(current.revision, original.revision);
    assert_eq!(current.document, original.document);

    let mut fresh = edited.clone();
    let mut profile = fresh.profile().unwrap().unwrap();
    profile.id = uuid::Uuid::new_v4();
    fresh.set_profile(&profile).unwrap();
    no_space(&store.put(&fresh, ExpectedRevision::Absent).unwrap_err());
    let snapshot = store.snapshot();
    assert!(snapshot.diagnostics.is_empty());
    assert_eq!(snapshot.documents.len(), 1);
    assert_eq!(snapshot.documents[0].revision, original.revision);
    let parent = root
        .join(original.key.relative_path())
        .parent()
        .unwrap()
        .to_owned();
    assert_eq!(
        fs::read_dir(parent).unwrap().count(),
        1,
        "failed writes left staging files"
    );

    fs::remove_file(filler).unwrap();
    File::open(&filesystem.mount).unwrap().sync_all().unwrap();
    let replacement = store
        .put(&edited, ExpectedRevision::Digest(&original.revision))
        .unwrap();
    drop(store);
    let reopened = KnowledgeStore::open(&root, false)
        .unwrap()
        .get(original.key)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.revision, replacement.revision);
    assert_eq!(reopened.document, edited);
    assert_ne!(reopened.revision, original.revision);
}
