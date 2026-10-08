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
