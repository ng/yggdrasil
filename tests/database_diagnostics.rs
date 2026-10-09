use sqlx::postgres::PgConnectOptions;
use ygg::db::diagnostics::{SessionObservation, inspect};

#[tokio::test]
async fn direct_database_reports_observations_without_certifying_pool_mode() {
    let options = ygg::db::external::options(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    let report = inspect(&options).await.unwrap();
    assert!(report.server_major >= 14);
    assert_eq!(report.session, SessionObservation::StableButUnverified);
    assert!(!report.observed_incompatibility());
    assert!(
        report
            .session_requirement
            .contains("Transaction pooling is unsupported")
    );
}

#[tokio::test]
async fn diagnostic_connection_errors_do_not_echo_credentials() {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(port)
        .username("diagnostic-secret-user")
        .password("diagnostic-secret-password");
    let error = inspect(&options).await.unwrap_err();
    assert!(!format!("{error:?}").contains("diagnostic-secret"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn cli_diagnostic_ignores_owner_and_creates_no_managed_state() {
    let temp = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
        .env("DATABASE_URL", std::env::var("DATABASE_URL").unwrap())
        .env("YGG_DB_MODE", "external")
        .env("YGG_CONFIG_DIR", temp.path().join("config"))
        .env("YGG_DATA_DIR", temp.path().join("data"))
        .env("YGG_DATABASE_OWNER_URL", "invalid-owner-secret")
        .args(["db", "diagnose", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["session"], "stable_but_unverified");
    assert!(!temp.path().join("data").exists());
    assert!(!temp.path().join("config").exists());
}

/// Exercise real transaction reassignment, not a mock of backend identity.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires YGG_TEST_PGBOUNCER_BIN and isolated DATABASE_URL"]
async fn pgbouncer_session_and_transaction_modes_have_distinct_observations() {
    use sqlx::{Connection, PgConnection, Row};
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    let upstream = ygg::db::external::options(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    let temp = tempfile::Builder::new()
        .prefix("ygg-pool-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let host = upstream
        .get_socket()
        .map(|path| path.to_str().unwrap())
        .unwrap_or(upstream.get_host());
    let mut target = format!(
        "host={} port={} dbname={} user={}",
        quote(host),
        upstream.get_port(),
        quote(upstream.get_database().unwrap_or(upstream.get_username())),
        quote(upstream.get_username())
    );
    if let Ok(password) = std::env::var("YGG_TEST_PGBOUNCER_PASSWORD") {
        if !password.is_empty() {
            target.push_str(&format!(" password={}", quote(&password)));
        }
    }
    let config = format!(
        "[databases]\nsession = {target} pool_mode=session\ntransaction = {target} pool_mode=transaction\n[pgbouncer]\nlisten_addr =\nlisten_port = 6432\nunix_socket_dir = {}\nauth_type = any\ndefault_pool_size = 2\nserver_round_robin = 1\nmax_prepared_statements = 100\nignore_startup_parameters = extra_float_digits\nlogfile = {}\npidfile = {}\n",
        root.display(),
        root.join("pool.log").display(),
        root.join("pool.pid").display()
    );
    std::fs::write(root.join("pool.ini"), config).unwrap();
    let mut process =
        tokio::process::Command::new(std::env::var("YGG_TEST_PGBOUNCER_BIN").unwrap())
            .arg(root.join("pool.ini"))
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(root.join("stderr.log")).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
    let options = PgConnectOptions::new()
        .host(root.to_str().unwrap())
        .port(6432)
        .username(upstream.get_username())
        .database("session");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                process.try_wait().unwrap().is_none(),
                "{}",
                std::fs::read_to_string(root.join("stderr.log")).unwrap_or_default()
            );
            if let Ok(connection) = PgConnection::connect_with(&options).await {
                connection.close().await.unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{}",
            std::fs::read_to_string(root.join("stderr.log")).unwrap_or_default()
        )
    });
    for (database, expected) in [
        ("session", SessionObservation::StableButUnverified),
        (
            "transaction",
            SessionObservation::BackendChangedIncompatible,
        ),
    ] {
        let options = options.clone().database(database);
        // Hold simultaneous transactions to populate two backend slots, then
        // release both. Round-robin forces transaction-mode reassignment.
        let mut first = PgConnection::connect_with(&options).await.unwrap();
        let mut second = PgConnection::connect_with(&options).await.unwrap();
        sqlx::raw_sql("BEGIN").execute(&mut first).await.unwrap();
        sqlx::raw_sql("BEGIN").execute(&mut second).await.unwrap();
        let a: i32 = sqlx::raw_sql("SELECT pg_backend_pid() AS pid")
            .fetch_one(&mut first)
            .await
            .unwrap()
            .get("pid");
        let b: i32 = sqlx::raw_sql("SELECT pg_backend_pid() AS pid")
            .fetch_one(&mut second)
            .await
            .unwrap()
            .get("pid");
        assert_ne!(a, b);
        sqlx::raw_sql("COMMIT").execute(&mut first).await.unwrap();
        sqlx::raw_sql("COMMIT").execute(&mut second).await.unwrap();
        first.close().await.unwrap();
        second.close().await.unwrap();
        let report = inspect(&options).await.unwrap();
        assert_eq!(report.session, expected);
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
            .env(
                "DATABASE_URL",
                format!(
                    "postgres://{}@localhost:6432/{database}?host={}",
                    upstream.get_username(),
                    root.display()
                ),
            )
            .env("YGG_DB_MODE", "external")
            .env_remove("YGG_DATABASE_OWNER_URL")
            .env("YGG_CONFIG_DIR", root.join("config"))
            .env("YGG_DATA_DIR", root.join("data"))
            .args(["db", "diagnose", "--json"])
            .output()
            .await
            .unwrap();
        assert_eq!(
            output.status.success(),
            database == "session",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["session"], serde_json::to_value(expected).unwrap());
        assert!(!root.join("data").exists());
        // The production authority guard must also reject reassignment before
        // polling any work. Session pooling retains authority on the same path.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        let key = uuid::Uuid::new_v4().as_u128() as i64;
        let mut guard = ygg::db::singleton::SingletonGuard::try_acquire(&pool, key)
            .await
            .unwrap()
            .unwrap();
        let mut reserved = None;
        if database == "transaction" {
            // Extended-protocol preparation can consume extra pool turns. Pin
            // the original backend on another client to force a real switch.
            let mut candidate = PgConnection::connect_with(&options).await.unwrap();
            sqlx::raw_sql("BEGIN")
                .execute(&mut candidate)
                .await
                .unwrap();
            let pid: i32 = sqlx::raw_sql("SELECT pg_backend_pid() AS pid")
                .fetch_one(&mut candidate)
                .await
                .unwrap()
                .get("pid");
            if pid != guard.backend_pid() {
                let mut original = PgConnection::connect_with(&options).await.unwrap();
                sqlx::raw_sql("BEGIN").execute(&mut original).await.unwrap();
                let pid: i32 = sqlx::raw_sql("SELECT pg_backend_pid() AS pid")
                    .fetch_one(&mut original)
                    .await
                    .unwrap()
                    .get("pid");
                assert_eq!(pid, guard.backend_pid());
                sqlx::raw_sql("COMMIT")
                    .execute(&mut candidate)
                    .await
                    .unwrap();
                candidate.close().await.unwrap();
                candidate = original;
            }
            reserved = Some(candidate);
        }
        let polled = std::sync::atomic::AtomicBool::new(false);
        let result = guard
            .supervise(async {
                polled.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert_eq!(result.is_ok(), database == "session");
        assert_eq!(
            polled.load(std::sync::atomic::Ordering::SeqCst),
            database == "session"
        );
        drop(guard);
        pool.close().await;
        if let Some(mut reserved) = reserved {
            sqlx::raw_sql("ROLLBACK")
                .execute(&mut reserved)
                .await
                .unwrap();
            reserved.close().await.unwrap();
        }
    }
    // Terminating our private pooler closes all upstream sessions, including
    // a lock stranded by the deliberately unsupported transaction-mode probe.
    process.kill().await.unwrap();
    process.wait().await.unwrap();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn managed_diagnostic_does_not_initialize_missing_cluster() {
    let temp = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
        .env_remove("DATABASE_URL")
        .env_remove("YGG_DATABASE_OWNER_URL")
        .env("YGG_DB_MODE", "managed")
        .env("YGG_CONFIG_DIR", temp.path().join("config"))
        .env("YGG_DATA_DIR", temp.path().join("data"))
        .args(["db", "diagnose", "--json"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not initialize"));
    assert!(!temp.path().join("data").exists());
}
