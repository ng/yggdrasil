//! Regression tests for `ygg remember` — durable OKF notes (ADR 0019, no
//! embeddings). Verifies prime/list retrieval semantics against the selected
//! corpus during database outages and healthy coordination.
//!
//! The healthy-coordination case requires Postgres + migrations applied:
//!     DATABASE_URL=postgres://ng@localhost:5432/ygg cargo test --test remember -- --test-threads=1

use std::env;
use uuid::Uuid;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn prime_keeps_fresh_local_notes_during_database_outage_and_session_hooks() {
    use ygg::knowledge::{identity::IdentityRegistry, runtime::Phase, store::KnowledgeStore};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    let mut ids = Vec::new();
    for n in 0..6 {
        let row = okf::json(
            okf::command(root, &root.join("repo"))
                .args([&format!("offline-note-{n}"), "--json"])
                .output()
                .unwrap(),
        );
        ids.push(Uuid::parse_str(row["memory_id"].as_str().unwrap()).unwrap());
    }
    okf::json(
        okf::command(root, root)
            .args(["offline-global", "--global", "--json"])
            .output()
            .unwrap(),
    );
    let prime = || {
        let output = okf::app(root, &root.join("worktree"))
            .arg("prime")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    let text = prime();
    assert!(text.contains("prime:degraded"));
    assert!(text.contains("handoff could not be loaded"));
    assert!(text.contains("offline-global"));
    assert!(text.contains("offline-note-5"));
    assert!(!text.contains("offline-note-0"));
    assert!(!text.contains("offline-note-1"));
    assert_eq!(
        text.lines().filter(|line| line.starts_with("  - ")).count(),
        5
    );
    let store = KnowledgeStore::open(&root.join("bundle"), false).unwrap();
    let deleted = store.find(ids[5]).unwrap().unwrap();
    store.delete(deleted.key, &deleted.revision).unwrap();
    let mut stale = store.find(ids[4]).unwrap().unwrap();
    stale
        .document
        .metadata
        .insert("status".into(), "deprecated".into());
    std::fs::write(
        root.join("bundle").join(stale.key.relative_path()),
        stale.document.serialize().unwrap(),
    )
    .unwrap();
    let text = prime();
    assert!(!text.contains("offline-note-5"));
    assert!(!text.contains("offline-note-4"));
    assert!(text.contains("offline-note-0"));
    assert_eq!(text.lines().filter(|l| l.starts_with("  - ")).count(), 5);
    let bad = store.find(ids[3]).unwrap().unwrap();
    std::fs::write(
        root.join("bundle").join(bad.key.relative_path()),
        "broken document",
    )
    .unwrap();
    let text = prime();
    assert!(text.contains("offline-global"));
    assert!(!text.contains("offline-note-3"));
    // Connection failures are bounded across the whole coordination read and redacted.
    let start = std::time::Instant::now();
    let out = okf::app(root, &root.join("repo"))
        .env(
            "DATABASE_URL",
            "postgres://hidden-user:never-print-this@127.0.0.1:1/absent",
        )
        .arg("prime")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("offline-global"));
    assert!(!text.contains("never-print-this"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("never-print-this"));
    for hook in ["session-start", "pre-compact"] {
        use std::io::Write;
        let mut child = okf::app(root, &root.join("repo"))
            .args(["hook", hook])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"{}").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("offline-global")
        );
    }
    let registry = IdentityRegistry::open(&root.join("policy"), false).unwrap();
    let (mut identities, revision) = registry.read().unwrap();
    identities.trusted = false;
    registry.replace(&revision, &identities).unwrap();
    assert!(!prime().contains("offline-global"));
    binding.phase = Phase::Fenced;
    okf::select(root, &binding);
    assert!(!prime().contains("offline-global"));
    assert!(
        !root.join("data").exists(),
        "prime bootstrapped a managed database"
    );
}

#[tokio::test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn prime_keeps_local_notes_when_coordination_is_healthy() {
    let db_url = env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    okf::json(
        okf::command(root, &root.join("repo"))
            .args(["healthy-local-note", "--json"])
            .output()
            .unwrap(),
    );
    let agent = format!("prime-fixture-{}", Uuid::new_v4());
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let output = okf::app(root, &root.join("repo"))
        .env("DATABASE_URL", &db_url)
        .env("YGG_AGENT_NAME", &agent)
        .arg("prime")
        .output()
        .unwrap();
    binding.phase = ygg::knowledge::runtime::Phase::Fenced;
    okf::select(root, &binding);
    let fenced = okf::app(root, &root.join("repo"))
        .env("DATABASE_URL", &db_url)
        .env("YGG_AGENT_NAME", &agent)
        .arg("prime")
        .output()
        .unwrap();
    sqlx::query("DELETE FROM agents WHERE agent_name=$1 AND user_id='legacy-user'")
        .bind(&agent)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("<!-- ygg:prime -->"), "{text}");
    assert!(text.contains("healthy-local-note"));
    assert!(fenced.status.success());
    let fenced = String::from_utf8(fenced.stdout).unwrap();
    assert!(fenced.contains("<!-- ygg:prime -->"));
    assert!(!fenced.contains("healthy-local-note"));
    assert!(!text.contains("prime:degraded"));
    assert!(!root.join("data").exists());
}
