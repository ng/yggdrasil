#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use ygg::db::runtime::{ManagedCluster, Status};

struct Cleanup {
    bin: PathBuf,
    root: PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new(self.bin.join("pg_ctl"))
            .arg("-D")
            .arg(self.root.join("data"))
            .args(["stop", "-m", "fast", "-w", "-t", "5"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

#[tokio::test]
#[ignore = "requires native YGG_TEST_PG_BIN and YGG_TEST_PG_MAJOR; owns a delayed helper and disposable cluster"]
async fn startup_probes_share_caller_deadline_without_hidden_five_second_limit() {
    let source = PathBuf::from(std::env::var("YGG_TEST_PG_BIN").unwrap())
        .canonicalize()
        .unwrap();
    let major = std::env::var("YGG_TEST_PG_MAJOR").unwrap().parse().unwrap();
    let temp = tempfile::Builder::new()
        .prefix("yprobe-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    let bin = base.join("bin");
    std::fs::DirBuilder::new().mode(0o700).create(&bin).unwrap();
    for entry in std::fs::read_dir(&source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "pg_controldata" {
            symlink(entry.path(), bin.join(entry.file_name())).unwrap();
        }
    }
    let delay = base.join("delay");
    let wrapper = bin.join("pg_controldata");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ -f {} ]; then /bin/sleep 6; fi\nexec {} \"$@\"\n",
            quote(&delay),
            quote(&source.join("pg_controldata"))
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = base.join("cluster");
    let cluster = ManagedCluster::initialize(&root, &bin, major)
        .await
        .unwrap();
    let _cleanup = Cleanup {
        bin,
        root: root.clone(),
    };
    let mut owner = cluster.try_owner().unwrap().unwrap();
    std::fs::write(&delay, b"owned test delay").unwrap();
    let start = Instant::now();
    owner.start_or_adopt(Duration::from_secs(30)).await.unwrap();
    assert!(start.elapsed() >= Duration::from_secs(6));
    assert!(matches!(
        cluster.status().await.unwrap(),
        Status::Ready { .. }
    ));
    // Reset only this owned fixture through its native helper. The separate
    // supervisor suite exercises stop-time identity checks; this test isolates
    // the startup deadline and must not depend on a second readiness probe.
    let stopped = Command::new(source.join("pg_ctl"))
        .arg("-D")
        .arg(root.join("data"))
        .args(["stop", "-m", "fast", "-w", "-t", "30"])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let start = Instant::now();
    let error = owner
        .start_or_adopt(Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("timed out"));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "caller deadline must bound all probes together"
    );
    assert_eq!(cluster.status().await.unwrap(), Status::Stopped);
}
