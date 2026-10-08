#![cfg(any(target_os = "macos", target_os = "linux"))]
use futures::FutureExt;
use sqlx::{postgres::PgConnectOptions, postgres::PgPoolOptions};
use std::{path::PathBuf, time::Duration};
use ygg::db::{package, runtime::ManagedCluster};

#[test]
fn manifest_and_checksum_failures_never_create_an_installation() {
    let manifest = package::release().unwrap();
    assert_eq!(manifest.postgres_version, "16.15");
    assert_eq!(manifest.source_commit.len(), 40);
    for target in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
    ] {
        let p = serde_json::to_value(package::package(target).unwrap()).unwrap();
        assert_eq!(p["sha256"].as_str().unwrap().len(), 64);
        assert!(p["url"].as_str().unwrap().contains("/16.15.0/"));
    }
    assert!(package::package("aarch64-unknown-linux-gnu").is_err());
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("bad.tar.gz");
    let destination = temp.path().join("binaries");
    std::fs::write(&archive, b"not an archive").unwrap();
    assert!(package::install_offline(&destination, &archive).is_err());
    assert!(!destination.exists());
    let native =
        serde_json::to_value(package::package(package::current_target().unwrap()).unwrap())
            .unwrap();
    let file = std::fs::File::create(&archive).unwrap();
    file.set_len(native["bytes"].as_u64().unwrap()).unwrap();
    assert!(
        package::install_offline(&destination, &archive)
            .unwrap_err()
            .to_string()
            .contains("SHA-256")
    );
    assert!(!destination.exists());
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_ARCHIVE pointing to the exact native pinned archive"]
async fn pinned_offline_package_runs_migrations_and_refuses_modified_installation() {
    let archive = PathBuf::from(std::env::var("YGG_TEST_PG_ARCHIVE").unwrap());
    let temp = tempfile::Builder::new()
        .prefix("ypkg-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap().join("binaries");
    let bin = package::install_offline(&base, &archive).unwrap();
    assert_eq!(package::install_offline(&base, &archive).unwrap(), bin);
    if std::env::var_os("YGG_TEST_PG_DOWNLOAD").is_some() {
        assert_eq!(package::install_download(&base).await.unwrap(), bin);
    }
    assert!(
        bin.parent()
            .unwrap()
            .join("YGG-THIRD-PARTY-NOTICES.txt")
            .is_file()
    );
    let root = temp.path().canonicalize().unwrap().join("cluster");
    let cluster = ManagedCluster::initialize(&root, &bin, 16).await.unwrap();
    let mut owner = cluster.try_owner().unwrap().unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        owner.start_or_adopt(Duration::from_secs(30)).await.unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(
                PgConnectOptions::new()
                    .host(root.join("runtime").to_str().unwrap())
                    .port(5432)
                    .username("ygg_bootstrap")
                    .database("postgres"),
            )
            .await
            .unwrap();
        ygg::db::run_migrations(&pool).await.unwrap();
        let uuid: uuid::Uuid = sqlx::query_scalar("SELECT uuid_generate_v4()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!uuid.is_nil());
        let version: String = sqlx::query_scalar("SHOW server_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(version.starts_with("16.15"), "{version}");
        // Concurrent explicit provisioning must converge on the same roles and
        // schema. Re-running must preserve database/knowledge identities.
        let (first, second) = tokio::join!(
            ygg::db::provision::migrate(&cluster),
            ygg::db::provision::migrate(&cluster)
        );
        first.unwrap();
        second.unwrap();
        let runtime = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(ygg::db::provision::runtime_options(&cluster))
            .await
            .unwrap();
        let user: String = sqlx::query_scalar("SELECT current_user")
            .fetch_one(&runtime)
            .await
            .unwrap();
        assert_eq!(user, "ygg_runtime");
        assert!(
            ygg::db::pending_migrations(&runtime)
                .await
                .unwrap()
                .is_empty()
        );
        let mut guarded = ygg::knowledge::guard::legacy_transaction(&runtime, true, None)
            .await
            .unwrap();
        sqlx::query("INSERT INTO memories (text) VALUES ('limited-role note')")
            .execute(&mut *guarded)
            .await
            .unwrap();
        guarded.commit().await.unwrap();
        for forbidden in [
            "CREATE TABLE public.forbidden (id integer)",
            "CREATE TEMP TABLE forbidden (id integer)",
            "CREATE DATABASE forbidden",
            "CREATE ROLE forbidden",
            "SET ROLE ygg_owner",
            "SET ROLE ygg_bootstrap",
            "UPDATE knowledge_storage SET generation = generation + 1",
            "DELETE FROM _sqlx_migrations",
            "ALTER TABLE memories DISABLE TRIGGER ALL",
            "TRUNCATE memories",
        ] {
            let error = sqlx::query(forbidden).execute(&runtime).await.unwrap_err();
            assert_eq!(
                error.as_database_error().and_then(|e| e.code()).as_deref(),
                Some("42501"),
                "{forbidden}: {error}"
            );
        }
        // Drift is reported, not silently repaired or adopted.
        sqlx::query("ALTER ROLE ygg_runtime SUPERUSER")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            ygg::db::provision::migrate(&cluster)
                .await
                .unwrap_err()
                .to_string()
                .contains("unexpected privilege")
        );
        sqlx::query("ALTER ROLE ygg_runtime NOSUPERUSER")
            .execute(&pool)
            .await
            .unwrap();
        runtime.close().await;
        pool.close().await;
    })
    .catch_unwind()
    .await;
    let stopped = owner.stop(Duration::from_secs(10)).await;
    if stopped.is_err() {
        let _ = tokio::process::Command::new(bin.join("pg_ctl"))
            .arg("-D")
            .arg(root.join("data"))
            .args(["stop", "-m", "fast", "-w", "-t", "5"])
            .output()
            .await;
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    stopped.unwrap();
    let edited = bin
        .parent()
        .unwrap()
        .join("share/extension/uuid-ossp.control");
    std::fs::write(&edited, "independent edit").unwrap();
    assert!(package::install_offline(&base, &archive).is_err());
    assert_eq!(std::fs::read_to_string(edited).unwrap(), "independent edit");
}
