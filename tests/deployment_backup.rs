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
        configuration: None,
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

#[tokio::test]
async fn configured_policy_cannot_be_silently_omitted_without_a_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let policy = temp.path().join("policy");
    std::fs::create_dir(&policy).unwrap();
    let destination = temp.path().join("backup");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
        .env("YGG_CONFIG_DIR", temp.path().join("config"))
        .env("YGG_DATA_DIR", temp.path().join("data"))
        .env("YGG_DB_MODE", "external")
        .env("DATABASE_URL", "postgres://unused@localhost:1/unused")
        .env_remove("YGG_DATABASE_OWNER_URL")
        .env("YGG_KNOWLEDGE_DIR", temp.path().join("absent-bundle"))
        .env("YGG_KNOWLEDGE_POLICY_DIR", &policy)
        .args(["db", "backup"])
        .arg(&destination)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to omit policy"));
    assert!(!destination.exists());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn version_two_configuration_is_private_integrity_bound_and_offline() {
    use std::collections::BTreeMap;
    use ygg::config::{database::DeploymentConfig, snapshot::Snapshot};
    use ygg::db::deployment_backup::ConfigurationRevision;
    let temp = tempfile::tempdir().unwrap();
    let backup = temp.path().join("backup");
    std::fs::create_dir(&backup).unwrap();
    fixture(&backup);
    let source = temp.path().join("source-config");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join(".env"),
        "YGG_DATA_DIR=${HOME}/configuration-fixture\nYGG_USER=fixture-secret-user\n",
    )
    .unwrap();
    let inputs = BTreeMap::from([
        ("YGG_CONFIG_DIR".into(), source.display().to_string()),
        ("HOME".into(), std::env::var("HOME").unwrap()),
        (
            "DATABASE_URL".into(),
            "postgres://user:fixture-secret-password@127.0.0.1/db".into(),
        ),
    ]);
    let config = DeploymentConfig::load_maintenance(inputs).unwrap();
    let snapshot = Snapshot::capture(&config, None).unwrap().encode().unwrap();
    let component = backup.join("configuration.json");
    std::fs::write(&component, &snapshot).unwrap();
    std::fs::set_permissions(&component, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut manifest = verify(&backup).err(); // Extra file is rejected until bound by v2.
    assert!(manifest.take().is_some());
    let mut saved: Manifest =
        serde_json::from_slice(&std::fs::read(backup.join("backup.json")).unwrap()).unwrap();
    saved.version = 2;
    saved.configuration = Some(ConfigurationRevision {
        bytes: snapshot.len() as u64,
        sha256: ygg::knowledge::document::digest(&snapshot),
    });
    std::fs::write(
        backup.join("backup.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    assert_eq!(verify(&backup).unwrap(), saved);
    std::fs::remove_file(source.join(".env")).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
        .args(["db", "verify-backup"])
        .arg(&backup)
        .arg("--json")
        .env("HOME", temp.path().join("different-home"))
        .env("YGG_DB_MODE", "invalid")
        .env("DATABASE_URL", "invalid-secret")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-secret"));
    std::fs::write(&component, b"changed").unwrap();
    assert!(verify(&backup).is_err());
    std::fs::write(&component, &snapshot).unwrap();
    std::fs::set_permissions(&component, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(verify(&backup).is_err());
}
