#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    fs::File,
    os::unix::fs::{PermissionsExt, symlink},
};
use ygg::db::{
    backup::DatabaseSnapshot,
    deployment_backup::{Manifest, verify},
};

fn fixture(root: &std::path::Path) {
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let data = b"PGDMPintegrity fixture only";
    std::fs::write(root.join("database.dump"), data).unwrap();
    let manifest = Manifest {
        version: 1,
        created_at: chrono::Utc::now(),
        knowledge: None,
        database: DatabaseSnapshot {
            validation: None,
            database_id: uuid::Uuid::new_v4(),
            generation: 1,
            backend: "sql".into(),
            corpus_id: None,
            server_major: 16,
            tool_version: "pg_dump (PostgreSQL) 16.15".into(),
            migrations: vec![],
            table_rows: Default::default(),
            bytes: data.len() as u64,
            sha256: ygg::knowledge::document::digest(data),
        },
    };
    std::fs::write(
        root.join("backup.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn offline_integrity_rejects_modified_missing_and_extra_components() {
    for mutation in [
        "dump",
        "extra",
        "missing",
        "binding",
        "symlink",
        "public-root",
    ] {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        assert!(verify(temp.path()).is_ok()); // Integrity is not archive/restore validation.
        match mutation {
            "dump" => std::fs::write(temp.path().join("database.dump"), b"PGDMPmodified").unwrap(),
            "extra" => {
                File::create(temp.path().join("unexpected")).unwrap();
            }
            "missing" => std::fs::remove_file(temp.path().join("database.dump")).unwrap(),
            "binding" => {
                let mut manifest: Manifest = serde_json::from_slice(
                    &std::fs::read(temp.path().join("backup.json")).unwrap(),
                )
                .unwrap();
                manifest.database.backend = "okf".into();
                manifest.database.corpus_id = Some(uuid::Uuid::new_v4());
                std::fs::write(
                    temp.path().join("backup.json"),
                    serde_json::to_vec(&manifest).unwrap(),
                )
                .unwrap();
            }
            "symlink" => {
                std::fs::rename(
                    temp.path().join("database.dump"),
                    temp.path().join("real.dump"),
                )
                .unwrap();
                symlink("real.dump", temp.path().join("database.dump")).unwrap();
            }
            _ => std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))
                .unwrap(),
        }
        assert!(verify(temp.path()).is_err(), "{mutation}");
    }
}

#[tokio::test]
async fn verify_cli_never_loads_database_configuration() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
        .env("YGG_DB_MODE", "invalid")
        .env("DATABASE_URL", "unusable-secret")
        .args(["db", "verify-backup"])
        .arg(temp.path())
        .arg("--json")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
}

#[tokio::test]
async fn restore_without_evidence_refuses_before_creating_target() {
    let temp = tempfile::tempdir().unwrap();
    let backup = temp.path().join("backup");
    std::fs::create_dir(&backup).unwrap();
    fixture(&backup);
    let data = temp.path().join("new-managed");
    let destination = temp.path().join("restored");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
        .env("YGG_DB_MODE", "managed")
        .env_remove("DATABASE_URL")
        .env_remove("YGG_DATABASE_OWNER_URL")
        .env("YGG_CONFIG_DIR", temp.path().join("config"))
        .env("YGG_DATA_DIR", &data)
        .args(["db", "restore"])
        .arg(&backup)
        .arg("--destination")
        .arg(&destination)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("lacks supported restore evidence"));
    assert!(!data.exists());
    assert!(!destination.exists());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}
