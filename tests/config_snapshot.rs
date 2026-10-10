#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    collections::BTreeMap,
    os::unix::{ffi::OsStrExt, fs::symlink},
};
use ygg::config::{database::DeploymentConfig, snapshot::Snapshot};
fn env(root: &std::path::Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("YGG_CONFIG_DIR".into(), root.display().to_string()),
        (
            "YGG_DATA_DIR".into(),
            root.join("data").display().to_string(),
        ),
        (
            "DATABASE_URL".into(),
            "postgres://runtime:fixture-password@127.0.0.1/database".into(),
        ),
        (
            "UNRELATED_SERVICE_SECRET".into(),
            "do-not-archive-unrelated".into(),
        ),
    ])
}
#[test]
fn snapshot_preserves_exact_inputs_and_overrides_but_redacts_debug() {
    let temp = tempfile::tempdir().unwrap();
    let toml = "# original comments\r\n[database]\r\nmode = 'external'\r\nurl = 'postgres://file-source/db'\r\n";
    let dotenv = "DATABASE_URL=postgres://dotenv-source/db\nYGG_USER=alice\nYGG_DB_POOL=12\n";
    std::fs::write(temp.path().join("config.toml"), toml).unwrap();
    std::fs::write(temp.path().join(".env"), dotenv).unwrap();
    let config = DeploymentConfig::load_maintenance(env(temp.path())).unwrap();
    let saved = Snapshot::capture(&config, None).unwrap();
    let bytes = saved.encode().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["config_toml"], toml);
    assert_eq!(value["dotenv"], dotenv);
    assert!(
        value["effective"]["url"]
            .as_str()
            .unwrap()
            .contains("fixture-password")
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("do-not-archive-unrelated"));
    assert!(!format!("{saved:?}").contains("fixture-password"));
    assert_eq!(Snapshot::decode(&bytes).unwrap(), saved);
    std::fs::write(temp.path().join(".env"), "YGG_USER=bob\n").unwrap();
    assert!(saved.verify_sources().is_err());
    // Archive integrity remains independent from the now-changed source.
    assert!(Snapshot::decode(&bytes).is_ok());
    let mut other = env(temp.path());
    other.insert("DATABASE_URL".into(), "postgres://other/db".into());
    assert!(
        saved
            .verify_selection(&DeploymentConfig::load_maintenance(other).unwrap())
            .is_err()
    );
}
#[test]
fn changed_selection_between_load_and_capture_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let mut inputs = env(temp.path());
    inputs.remove("DATABASE_URL");
    std::fs::write(
        temp.path().join(".env"),
        "DATABASE_URL=postgres://first/db\n",
    )
    .unwrap();
    let config = DeploymentConfig::load_maintenance(inputs).unwrap();
    std::fs::write(
        temp.path().join(".env"),
        "DATABASE_URL=postgres://second/db\n",
    )
    .unwrap();
    assert!(Snapshot::capture(&config, None).is_err());
}
#[test]
fn maintenance_input_is_bounded_nonblocking_and_rejects_links() {
    for kind in ["symlink", "hardlink", "fifo", "oversize"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        match kind {
            "symlink" => {
                std::fs::write(temp.path().join("target"), "YGG_USER=alice").unwrap();
                symlink("target", &path).unwrap();
            }
            "hardlink" => {
                std::fs::write(temp.path().join("target"), "YGG_USER=alice").unwrap();
                std::fs::hard_link(temp.path().join("target"), &path).unwrap();
            }
            "fifo" => {
                let p = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(p.as_ptr(), 0o600) }, 0);
            }
            _ => std::fs::write(&path, vec![b'x'; 1024 * 1024 + 1]).unwrap(),
        }
        assert!(
            DeploymentConfig::load_maintenance(env(temp.path())).is_err(),
            "{kind}"
        );
    }
}
#[test]
fn malformed_configuration_errors_never_echo_contents() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("config.toml"),
        "private-secret-value = [malformed",
    )
    .unwrap();
    let error = DeploymentConfig::load_maintenance(env(temp.path()))
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("private-secret-value"));
    let error = Snapshot::decode(br#"{"version":"private-secret-value"}"#).unwrap_err();
    assert!(!format!("{error:#}").contains("private-secret-value"));
}

#[test]
fn recursive_dotenv_expansion_is_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let key = format!("YGG_CONFIG_EXPANSION_{}", uuid::Uuid::new_v4().simple());
    let mut dotenv = format!("{key}=x\n");
    for _ in 0..25 {
        dotenv.push_str(&format!("{key}=${{{key}}}${{{key}}}\n"));
    }
    std::fs::write(temp.path().join(".env"), dotenv).unwrap();
    assert!(DeploymentConfig::load_maintenance(env(temp.path())).is_err());
}

#[test]
fn forged_configuration_origin_and_unbound_values_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let config = DeploymentConfig::load_maintenance(env(temp.path())).unwrap();
    let bytes = Snapshot::capture(&config, None).unwrap().encode().unwrap();
    let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let mut changed = original.clone();
    changed["origin"]["directory"] = serde_json::json!(temp.path().join("elsewhere"));
    assert!(Snapshot::decode(&serde_json::to_vec(&changed).unwrap()).is_err());
    let mut changed = original;
    changed["dotenv_values"]["YGG_USER"] = serde_json::json!("invented");
    assert!(Snapshot::decode(&serde_json::to_vec(&changed).unwrap()).is_err());
    let mut inputs = env(temp.path());
    inputs.remove("DATABASE_URL");
    let mut managed = DeploymentConfig::load_maintenance(inputs).unwrap();
    managed.data_dir = temp.path().join("different-data");
    assert!(Snapshot::capture(&managed, None).is_err());
}
