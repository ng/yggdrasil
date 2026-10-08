#![cfg(any(target_os = "macos", target_os = "linux"))]
use futures::FutureExt;
use serde_json::Value;
use sqlx::{Connection, PgConnection, postgres::PgConnectOptions};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    time::{sleep, timeout},
};
use ygg::db::{
    runtime::{ManagedCluster, Status},
    supervisor,
};

const WAIT: Duration = Duration::from_secs(30);

fn app(data: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ygg"));
    cmd.env("YGG_DB_MODE", "managed")
        .env("YGG_DATA_DIR", data)
        .env("YGG_CONFIG_DIR", data.join("config"))
        .env_remove("DATABASE_URL")
        .env_remove("YGG_PROFILE")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

fn cli(data: &Path) -> Command {
    let mut cmd = app(data);
    cmd.arg("db");
    cmd
}

async fn success(cmd: &mut Command) -> String {
    let output = timeout(WAIT, cmd.output()).await.unwrap().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn json(cmd: &mut Command) -> Value {
    let output = timeout(WAIT, cmd.output()).await.unwrap().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn status_is_read_only_and_external_lifecycle_is_rejected_without_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::write(
        temp.path().join(".env"),
        "DATABASE_URL=postgres://repository-wrong-target/db\n",
    )
    .unwrap();
    let status = json(
        cli(&data)
            .current_dir(temp.path())
            .args(["status", "--json"]),
    )
    .await;
    assert_eq!(status["state"], "not_initialized");
    assert!(!data.exists());
    assert!(
        !app(&data)
            .args(["migrate", "--check"])
            .output()
            .await
            .unwrap()
            .status
            .success()
    );
    assert!(!data.exists());
    for action in ["start", "stop", "serve"] {
        assert!(
            !cli(&data)
                .arg(action)
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert!(!data.exists());
    }
    let url = "postgres://private-user:do-not-print@127.0.0.1:1/not-a-server";
    let failed_external = app(&data)
        .env("YGG_DB_MODE", "external")
        .env("DATABASE_URL", url)
        .args(["migrate", "--check"])
        .output()
        .await
        .unwrap();
    assert!(!failed_external.status.success());
    assert!(!String::from_utf8_lossy(&failed_external.stderr).contains("do-not-print"));
    assert!(
        !data.exists(),
        "failed external connections must not initialize managed state"
    );
    let status = json(
        cli(&data)
            .env("YGG_DB_MODE", "external")
            .env("DATABASE_URL", url)
            .args(["status", "--json"]),
    )
    .await;
    assert_eq!(status["mode"], "external");
    assert_eq!(status["state"], "configured");
    assert!(!status.to_string().contains("do-not-print"));
    for action in ["start", "stop", "serve"] {
        let output = cli(&data)
            .env("YGG_DB_MODE", "external")
            .env("DATABASE_URL", url)
            .arg(action)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("external database selected"), "{error}");
        assert!(!error.contains("do-not-print"));
        assert!(!data.exists());
    }
    assert!(
        !cli(&data)
            .args(["start", "--timeout", "0"])
            .output()
            .await
            .unwrap()
            .status
            .success()
    );
    assert!(!data.exists());
}

async fn connection(root: &Path) -> PgConnection {
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
#[ignore = "requires YGG_TEST_PG_BIN and YGG_TEST_PG_MAJOR; disposable native cluster"]
async fn real_cli_supervises_adopts_recovers_and_drains_without_restarting_after_stop() {
    let bin = PathBuf::from(std::env::var("YGG_TEST_PG_BIN").unwrap());
    let major = std::env::var("YGG_TEST_PG_MAJOR").unwrap().parse().unwrap();
    let temp = tempfile::Builder::new()
        .prefix("ysv-")
        .tempdir_in("/tmp")
        .unwrap();
    let data = temp.path().canonicalize().unwrap();
    let root = data.join("postgres");
    let cluster = ManagedCluster::initialize(&root, &bin, major)
        .await
        .unwrap();
    let mut supervisor_ids = Vec::new();
    let result = std::panic::AssertUnwindSafe(async {
        let stopped = json(cli(&data).args(["status", "--json"])).await;
        assert_eq!(stopped["postgres"]["state"], "stopped");
        assert!(!root.join("data/postmaster.pid").exists());
        let mut clients = Vec::new();
        for _ in 0..20 {
            clients.push(cli(&data).args(["start", "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap());
        }
        let mut responses = Vec::new();
        for client in clients {
            let output = timeout(WAIT, client.wait_with_output()).await.unwrap().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            responses.push(serde_json::from_slice::<supervisor::Reply>(&output.stdout).unwrap());
        }
        let first = &responses[0];
        supervisor_ids.push(first.supervisor_pid);
        assert!(responses.iter().all(|r| r.supervisor_pid == first.supervisor_pid && r.postgres == first.postgres));
        let Status::Ready { pid } = first.postgres else { panic!("not ready") };
        assert_eq!(cluster.status().await.unwrap(), first.postgres);
        // Real application commands use the resolver and limited runtime role;
        // explicit migrate selects the owner even before runtime DB exists.
        success(app(&data).arg("migrate")).await;
        success(app(&data).args(["migrate", "--check"])).await;
        success(app(&data).args(["remember", "runtime CLI note", "--global"])).await;
        let notes = success(app(&data).args(["remember", "--list", "--global", "--json"])).await;
        assert!(notes.contains("runtime CLI note"));
        let mut conn = connection(&root).await;
        sqlx::raw_sql("CREATE TABLE durable(value text); INSERT INTO durable VALUES('retained')").execute(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        // Wrong corpus identity and overlong requests must not stop the server.
        for bytes in [format!("{{\"version\":1,\"cluster_id\":\"{}\",\"action\":{{\"Stop\":{{\"seconds\":1}}}}}}\n", uuid::Uuid::new_v4()).into_bytes(), vec![b'x'; 5000]] {
            let mut stream = tokio::net::UnixStream::connect(root.join("runtime/control.sock")).await.unwrap();
            let _ = stream.write_all(&bytes).await;
        }
        assert_eq!(cluster.status().await.unwrap(), Status::Ready { pid });
        assert_eq!(unsafe { libc::kill(first.supervisor_pid as i32, libc::SIGKILL) }, 0);
        timeout(WAIT, async {
            while cluster.try_owner().unwrap().is_none() { sleep(Duration::from_millis(50)).await; }
        }).await.unwrap();
        let adopted: supervisor::Reply = serde_json::from_value(json(cli(&data).args(["start", "--json"])).await).unwrap();
        supervisor_ids.push(adopted.supervisor_pid);
        assert_ne!(adopted.supervisor_pid, first.supervisor_pid);
        assert_eq!(adopted.postgres, Status::Ready { pid });
        // The persistent owner recovers its crashed child without a new CLI start.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let replacement = timeout(WAIT, async {
            loop {
                if let Ok(Status::Ready { pid: current }) = cluster.status().await {
                    if current != pid { break current; }
                }
                sleep(Duration::from_millis(50)).await;
            }
        }).await.unwrap();
        let mut conn = connection(&root).await;
        let value: String = sqlx::query_scalar("SELECT value FROM durable").fetch_one(&mut conn).await.unwrap();
        assert_eq!(value, "retained");
        let mut stopping = cli(&data).args(["stop", "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        sleep(Duration::from_millis(300)).await;
        assert!(stopping.try_wait().unwrap().is_none());
        conn.close().await.unwrap();
        let output = timeout(WAIT, stopping.wait_with_output()).await.unwrap().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        // A normal command also starts an initialized, stopped deployment.
        success(app(&data).args(["migrate", "--check"])).await;
        // An immediate start must not get stranded behind the exiting owner.
        let restarted: supervisor::Reply = serde_json::from_value(json(cli(&data).args(["start", "--json"])).await).unwrap();
        supervisor_ids.push(restarted.supervisor_pid);
        assert_ne!(restarted.postgres, Status::Ready { pid: replacement });
        let conn = connection(&root).await;
        let output = cli(&data).args(["stop", "--timeout", "1"]).output().await.unwrap();
        assert!(!output.status.success(), "held client should prevent smart-stop completion");
        conn.close().await.unwrap();
        timeout(WAIT, async {
            while cluster.status().await.unwrap_or(Status::Unverified) != Status::Stopped {
                sleep(Duration::from_millis(50)).await;
            }
        }).await.unwrap();
        sleep(Duration::from_millis(1500)).await;
        assert_eq!(cluster.status().await.unwrap(), Status::Stopped);
        assert!(cluster.try_owner().unwrap().is_some());
        assert_eq!(json(cli(&data).args(["status", "--json"])).await["postgres"]["state"], "stopped");
        // Unexpected filesystem content is retained, never replaced as a socket.
        std::fs::write(root.join("runtime/control.sock"), "preserve").unwrap();
        let output = cli(&data).args(["start", "--timeout", "3"]).output().await.unwrap();
        assert!(!output.status.success());
        assert_eq!(std::fs::read_to_string(root.join("runtime/control.sock")).unwrap(), "preserve");
        assert_eq!(cluster.status().await.unwrap(), Status::Stopped);
        std::fs::remove_file(root.join("runtime/control.sock")).unwrap();
        // A restored/replaced data directory must match the enrolled system ID
        // before a postmaster is launched, not merely fail SQL verification later.
        let saved = std::fs::read(root.join("cluster.json")).unwrap();
        let mut manifest: Value = serde_json::from_slice(&saved).unwrap();
        manifest["system_id"] = "1".into();
        std::fs::write(root.join("cluster.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();
        let output = cli(&data).args(["start", "--timeout", "3"]).output().await.unwrap();
        assert!(!output.status.success());
        assert!(!root.join("data/postmaster.pid").exists());
        std::fs::write(root.join("cluster.json"), saved).unwrap();
    }).catch_unwind().await;
    let _ = supervisor::stop(&cluster, Duration::from_secs(5)).await;
    // Failure cleanup only signals an observed supervisor still running this
    // test's exact root. Never trust a saved PID alone after a process exits.
    for pid in supervisor_ids {
        let process = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .await
            .unwrap();
        let command = String::from_utf8_lossy(&process.stdout);
        if command.contains("db serve --cluster-root") && command.contains(root.to_str().unwrap()) {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }
    let _ = Command::new(bin.join("pg_ctl"))
        .arg("-D")
        .arg(root.join("data"))
        .args(["stop", "-m", "fast", "-w", "-t", "5"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
