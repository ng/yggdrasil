#![cfg(any(target_os = "macos", target_os = "linux"))]
//! Opt-in lifecycle spike against explicitly supplied native binaries. These
//! tests initialize only new disposable roots; no existing cluster is enrolled.
use sqlx::{Connection, PgConnection, postgres::PgConnectOptions};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use ygg::db::runtime::{ManagedCluster, Status};

const WAIT: Duration = Duration::from_secs(30);

#[test]
fn runtime_worker() {
    let Ok(root) = std::env::var("YGG_RUNTIME_WORKER_ROOT") else {
        return;
    };
    let index = std::env::var("YGG_RUNTIME_WORKER_INDEX").unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let cluster = ManagedCluster::open(Path::new(&root)).unwrap();
        let mut owner = cluster.try_owner().unwrap();
        let pid = match owner.as_mut() {
            Some(owner) => owner.start_or_adopt(WAIT).await.unwrap(),
            None => cluster.wait_ready(WAIT).await.unwrap(),
        };
        std::fs::write(
            Path::new(&root)
                .parent()
                .unwrap()
                .join(format!("ready-{index}")),
            format!("{pid} {}", owner.is_some()),
        )
        .unwrap();
        if owner.is_some() {
            // The test kills this owner to exercise OS lease release. A bounded
            // lifetime also avoids abandoned helper processes after test failure.
            tokio::time::sleep(Duration::from_secs(90)).await;
        }
    });
}

struct Cleanup {
    children: Vec<Child>,
    bin: PathBuf,
    root: PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Only the freshly initialized test cluster, never DATABASE_URL.
        let _ = Command::new(self.bin.join("pg_ctl"))
            .arg("-D")
            .arg(self.root.join("data"))
            .args(["stop", "-m", "fast", "-w", "-t", "5"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

async fn connect(root: &Path) -> PgConnection {
    PgConnection::connect_with(
        &PgConnectOptions::new()
            .host(root.join("runtime").to_str().unwrap())
            .port(5432)
            .username("ygg_bootstrap")
            .database("postgres"),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires YGG_TEST_PG_BIN and YGG_TEST_PG_MAJOR; initializes disposable native cluster"]
async fn twenty_processes_adopt_and_recover_without_losing_commits() {
    let bin = PathBuf::from(std::env::var("YGG_TEST_PG_BIN").unwrap());
    let major = std::env::var("YGG_TEST_PG_MAJOR").unwrap().parse().unwrap();
    let temp = tempfile::Builder::new()
        .prefix("yrt-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap().join("cluster");
    let cluster = ManagedCluster::initialize(&root, &bin, major)
        .await
        .unwrap();
    let mut cleanup = Cleanup {
        children: Vec::new(),
        bin: bin.clone(),
        root: root.clone(),
    };
    assert_eq!(cluster.status().await.unwrap(), Status::Stopped);
    assert!(!root.join("data/postmaster.pid").exists());
    assert_eq!(ManagedCluster::open(&root).unwrap().id(), cluster.id());
    assert!(
        ManagedCluster::initialize(&root, &bin, major)
            .await
            .is_err()
    );
    for index in 0..20 {
        cleanup.children.push(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime_worker", "--nocapture"])
                .env("YGG_RUNTIME_WORKER_ROOT", &root)
                .env("YGG_RUNTIME_WORKER_INDEX", index.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
    }
    tokio::time::timeout(WAIT, async {
        while !(0..20).all(|index| temp.path().join(format!("ready-{index}")).exists()) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let ready: Vec<_> = (0..20)
        .map(|index| std::fs::read_to_string(temp.path().join(format!("ready-{index}"))).unwrap())
        .collect();
    let owners: Vec<_> = ready
        .iter()
        .enumerate()
        .filter(|(_, text)| text.ends_with("true"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(owners.len(), 1);
    let pid: i32 = ready[0].split_whitespace().next().unwrap().parse().unwrap();
    assert!(
        ready
            .iter()
            .all(|text| text.split_whitespace().next().unwrap() == pid.to_string())
    );
    assert!(cluster.try_owner().unwrap().is_none());
    let mut conn = connect(&root).await;
    sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"; CREATE TABLE durable(value text); INSERT INTO durable VALUES('acknowledged')")
        .execute(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    // Every short-lived non-owner CLI exits without stopping the postmaster.
    for (index, child) in cleanup.children.iter_mut().enumerate() {
        if index != owners[0] {
            assert!(child.wait().unwrap().success());
        }
    }
    assert_eq!(cluster.status().await.unwrap(), Status::Ready { pid });
    cleanup.children[owners[0]].kill().unwrap();
    cleanup.children[owners[0]].wait().unwrap();
    let mut owner = cluster.try_owner().unwrap().unwrap();
    assert_eq!(owner.start_or_adopt(WAIT).await.unwrap(), pid);
    // Actual server crash, followed by its native WAL/stale-PID recovery.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    tokio::time::timeout(WAIT, async {
        while cluster.status().await.unwrap() != Status::Stopped {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let replacement = owner.start_or_adopt(WAIT).await.unwrap();
    assert_ne!(replacement, pid);
    let mut conn = connect(&root).await;
    let value: String = sqlx::query_scalar("SELECT value FROM durable")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(value, "acknowledged");
    conn.close().await.unwrap();
    // Also recover when the current owner holds the Child handle: reap the
    // crashed child before treating its stale PID as reusable.
    assert_eq!(unsafe { libc::kill(replacement, libc::SIGKILL) }, 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let replacement = owner.start_or_adopt(WAIT).await.unwrap();
    let mut conn = connect(&root).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM durable")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(count, 1);
    conn.close().await.unwrap();
    drop(owner);
    assert_eq!(
        cluster.status().await.unwrap(),
        Status::Ready { pid: replacement }
    );
    let mut owner = cluster.try_owner().unwrap().unwrap();
    let conn = connect(&root).await;
    let stopping = tokio::spawn(async move { owner.stop(WAIT).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !stopping.is_finished(),
        "smart stop must wait for an existing client"
    );
    conn.close().await.unwrap();
    stopping.await.unwrap().unwrap();
    assert_eq!(cluster.status().await.unwrap(), Status::Stopped);
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and YGG_TEST_PG_MAJOR; initializes disposable native cluster"]
async fn refuses_wrong_major_and_unrelated_live_pid() {
    let bin = PathBuf::from(std::env::var("YGG_TEST_PG_BIN").unwrap());
    let major: u32 = std::env::var("YGG_TEST_PG_MAJOR").unwrap().parse().unwrap();
    let temp = tempfile::Builder::new()
        .prefix("yrt-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap().join("cluster");
    assert!(
        ManagedCluster::initialize(&root, &bin, major + 1)
            .await
            .is_err()
    );
    assert!(!root.join("data").exists());
    let cluster = ManagedCluster::initialize(&root, &bin, major)
        .await
        .unwrap();
    let false_pid = format!(
        "{}\n{}\n1\n5432\n{}\n\n0\nready\n",
        std::process::id(),
        root.join("data").display(),
        root.join("runtime").display()
    );
    std::fs::write(root.join("data/postmaster.pid"), &false_pid).unwrap();
    assert_eq!(cluster.status().await.unwrap(), Status::Unverified);
    let mut owner = cluster.try_owner().unwrap().unwrap();
    assert!(
        owner
            .start_or_adopt(Duration::from_millis(200))
            .await
            .is_err()
    );
    assert!(owner.stop(Duration::from_secs(1)).await.is_err());
    assert_eq!(
        std::fs::read_to_string(root.join("data/postmaster.pid")).unwrap(),
        false_pid
    );
    // Remove only this fixture's fabricated PID file; no server was started.
    std::fs::remove_file(root.join("data/postmaster.pid")).unwrap();
    let moved = root.with_file_name("moved");
    std::fs::rename(&root, &moved).unwrap();
    assert!(ManagedCluster::open(&moved).is_err());
    std::fs::rename(&moved, &root).unwrap();
    std::fs::write(root.join("data/PG_VERSION"), (major + 1).to_string()).unwrap();
    assert!(ManagedCluster::open(&root).is_err());
}
