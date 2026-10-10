#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;
use ygg::knowledge::document::digest;

#[test]
fn fleet_requests_fail_before_journal_creation_or_database_access() {
    let root = tempfile::tempdir().unwrap();
    let plan = root.path().join("plan.json");
    let journal = root.path().join("journal");
    for (bytes, hash, error) in [
        (
            b"{}".to_vec(),
            "0".repeat(64),
            "differs from supplied SHA-256",
        ),
        (b"{}".to_vec(), digest(b"{}"), "missing field"),
        (
            vec![b' '; 8 * 1024 * 1024 + 1],
            "0".repeat(64),
            "exceeds 8388608 bytes",
        ),
    ] {
        std::fs::write(&plan, bytes).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ygg"))
            .env("DATABASE_URL", "postgres://127.0.0.1:1/unavailable")
            .env(
                "YGG_DATABASE_OWNER_URL",
                "postgres://127.0.0.1:1/unavailable",
            )
            .args(["knowledge", "fleet", "--plan"])
            .arg(&plan)
            .arg("--journal")
            .arg(&journal)
            .args(["--request-sha256", &hash, "--json", "execute"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(error),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!journal.exists());
    }
}
#[test]
fn fleet_help_exposes_recovery_without_connecting() {
    let output = Command::new(env!("CARGO_BIN_EXE_ygg"))
        .env("DATABASE_URL", "invalid")
        .args(["knowledge", "fleet", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for action in [
        "status",
        "prepare",
        "execute",
        "cancel",
        "abort",
        "rollback",
        "cancel-rollback",
        "reconcile-rollback",
    ] {
        assert!(help.contains(action));
    }
}
