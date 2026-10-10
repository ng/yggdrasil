//! Regression tests for `ygg remember` — durable notes (post-ADR-0015, no
//! embeddings). Verifies repo-scoped vs global write and the prime/list
//! retrieval semantics (repo notes + global, newest-first).
//!
//! Requires Postgres + migrations applied:
//!     DATABASE_URL=postgres://ng@localhost:5432/ygg cargo test --test remember -- --test-threads=1

use std::env;
use uuid::Uuid;
use ygg::models::memory::MemoryRepo;
use ygg::models::repo::RepoRepo;

async fn make_repo(pool: &sqlx::PgPool, prefix: &str) -> Uuid {
    RepoRepo::new(pool)
        .register(None, prefix, prefix, Some(&format!("/tmp/{prefix}")))
        .await
        .unwrap()
        .repo_id
}

async fn teardown(pool: &sqlx::PgPool, repo_ids: &[Uuid]) {
    // memories cascade on repo delete; global notes are removed explicitly.
    for id in repo_ids {
        sqlx::query("DELETE FROM repos WHERE repo_id = $1")
            .bind(id)
            .execute(pool)
            .await
            .ok();
    }
}

#[tokio::test]
async fn repo_scoped_list_includes_global_notes() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let repo_a = make_repo(&pool, "memrepoa").await;
    let repo_b = make_repo(&pool, "memrepob").await;
    let mem = MemoryRepo::new(&pool);

    let a_note = mem.create(Some(repo_a), "note in A", None).await.unwrap();
    let b_note = mem.create(Some(repo_b), "note in B", None).await.unwrap();
    let g_note = mem.create(None, "global note", None).await.unwrap();

    // Repo A's view: its own note + the global one, never repo B's.
    let view = mem.list(Some(repo_a), false, 50).await.unwrap();
    let ids: Vec<Uuid> = view.iter().map(|m| m.memory_id).collect();
    assert!(ids.contains(&a_note.memory_id), "repo A note must appear");
    assert!(ids.contains(&g_note.memory_id), "global note must appear");
    assert!(
        !ids.contains(&b_note.memory_id),
        "repo B note must not leak into repo A's view"
    );

    // Clean up the global note (not cascaded by repo delete).
    mem.delete(g_note.memory_id).await.unwrap();
    teardown(&pool, &[repo_a, repo_b]).await;
}

#[tokio::test]
async fn all_flag_crosses_repos_and_newest_first() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let repo_a = make_repo(&pool, "memalla").await;
    let repo_b = make_repo(&pool, "memallb").await;
    let mem = MemoryRepo::new(&pool);

    let first = mem.create(Some(repo_a), "older", None).await.unwrap();
    let second = mem.create(Some(repo_b), "newer", None).await.unwrap();

    let all = mem.list(None, true, 50).await.unwrap();
    let ids: Vec<Uuid> = all.iter().map(|m| m.memory_id).collect();
    assert!(ids.contains(&first.memory_id));
    assert!(ids.contains(&second.memory_id));

    // Newest-first ordering: `second` precedes `first` in the result.
    let pos_first = ids.iter().position(|id| *id == first.memory_id).unwrap();
    let pos_second = ids.iter().position(|id| *id == second.memory_id).unwrap();
    assert!(
        pos_second < pos_first,
        "list must be newest-first (created_at DESC)"
    );

    teardown(&pool, &[repo_a, repo_b]).await;
}

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
    let old_note = format!("frozen-sql-note-{}", Uuid::new_v4());
    let old_id: Uuid = sqlx::query_scalar(
        "INSERT INTO memories(text,user_id) VALUES($1,'legacy-user') RETURNING memory_id",
    )
    .bind(&old_note)
    .fetch_one(&pool)
    .await
    .unwrap();
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
    sqlx::query("DELETE FROM memories WHERE memory_id=$1")
        .bind(old_id)
        .execute(&pool)
        .await
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
    assert!(
        !text.contains(&old_note),
        "selected OKF also read frozen SQL notes"
    );
    assert!(fenced.status.success());
    let fenced = String::from_utf8(fenced.stdout).unwrap();
    assert!(fenced.contains("<!-- ygg:prime -->"));
    assert!(!fenced.contains("healthy-local-note"));
    assert!(
        !fenced.contains(&old_note),
        "fenced OKF fell back to SQL notes"
    );
    assert!(!text.contains("prime:degraded"));
    assert!(!root.join("data").exists());
}
