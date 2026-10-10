#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use std::{
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::{Child, Stdio},
};
use uuid::Uuid;
use ygg::knowledge::{identity::IdentityRegistry, store::KnowledgeStore};

fn start(root: &Path, cwd: &Path, session: &str, tool: &str) -> Child {
    start_using(root, cwd, session, tool, None)
}
fn start_using(
    root: &Path,
    cwd: &Path,
    session: &str,
    tool: &str,
    database: Option<&str>,
) -> Child {
    let mut command = okf::app(root, cwd);
    if let Some(database) = database {
        command.env("DATABASE_URL", database);
    }
    let mut child = command
        .args(["hook", "pre-tool-use"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(serde_json::to_string(&serde_json::json!({
        "session_id": session, "tool_name": tool, "tool_input": { "file_path": "src/example.rs" }
    })).unwrap().as_bytes()).unwrap();
    child
}
fn finish(child: Child) -> String {
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    if out.stdout.is_empty() {
        return String::new();
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_owned()
}
#[test]
fn offline_edit_hooks_deduplicate_concurrently_and_revalidate_approval() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    let row = okf::json(
        okf::learn(root)
            .args([
                "create",
                "scoped-visible",
                "--file-glob",
                "src/*.rs",
                "--scope",
                "agent=fixture-agent",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    okf::json(
        okf::learn(root)
            .args(["create", "general-omit", "--json"])
            .output()
            .unwrap(),
    );
    okf::json(
        okf::learn(root)
            .args([
                "create",
                "wrong-agent-omit",
                "--file-glob",
                "src/*.rs",
                "--scope",
                "agent=someone-else",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    okf::json(
        okf::learn(root)
            .args([
                "propose",
                "pending-omit",
                "--file-glob",
                "src/*.rs",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let session = "race/../session Ω";
    let children: Vec<_> = (0..20)
        .map(|_| start(root, &root.join("worktree"), session, "Edit"))
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(finish)
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(outputs.len(), 1, "concurrent hooks emitted duplicate rules");
    assert!(outputs[0].contains("scoped-visible"));
    assert!(!outputs[0].contains("-omit"));
    assert!(finish(start(root, &root.join("repo"), session, "Write")).is_empty());
    assert!(finish(start(root, &root.join("repo"), "read-session", "Read")).is_empty());
    assert!(!finish(start(root, &root.join("repo"), "a/b", "NotebookEdit")).is_empty());
    assert!(
        !finish(start(root, &root.join("repo"), "a_b", "Edit")).is_empty(),
        "sanitized IDs collided"
    );
    let sessions = root.join("policy/.sessions");
    assert_eq!(
        std::fs::metadata(&sessions).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in std::fs::read_dir(&sessions).unwrap() {
        let path = entry.unwrap().path();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        if path.extension().is_some_and(|s| s == "json") {
            assert_eq!(path.file_stem().unwrap().len(), 64);
            assert!(!std::fs::read_to_string(&path).unwrap().contains(session));
        }
    }
    let id = Uuid::parse_str(row["learning_id"].as_str().unwrap()).unwrap();
    let store = KnowledgeStore::open(&root.join("bundle"), false).unwrap();
    let mut doc = store.find(id).unwrap().unwrap();
    doc.document.body = "revised-visible".into();
    std::fs::write(
        root.join("bundle").join(doc.key.relative_path()),
        doc.document.serialize().unwrap(),
    )
    .unwrap();
    assert!(finish(start(root, &root.join("repo"), "after-edit", "Edit")).is_empty());
    okf::success(
        okf::learn(root)
            .env_remove("YGG_AGENT_NAME")
            .args(["approve", &id.to_string()])
            .output()
            .unwrap(),
    );
    assert!(finish(start(root, &root.join("repo"), session, "Edit")).contains("revised-visible"));
    assert!(finish(start(root, &root.join("repo"), session, "Edit")).is_empty());
    let registry = IdentityRegistry::open(&root.join("policy"), false).unwrap();
    let (mut policy, revision) = registry.read().unwrap();
    policy.trusted = false;
    registry.replace(&revision, &policy).unwrap();
    assert!(finish(start(root, &root.join("repo"), "untrusted", "Edit")).is_empty());
    let (mut policy, revision) = registry.read().unwrap();
    policy.trusted = true;
    registry.replace(&revision, &policy).unwrap();
    let doc = store.find(id).unwrap().unwrap();
    store.delete(doc.key, &doc.revision).unwrap();
    assert!(finish(start(root, &root.join("repo"), "deleted", "Edit")).is_empty());
    assert!(
        !root.join("data").exists(),
        "offline matching touched managed state"
    );
}

#[test]
fn session_cache_failure_cannot_hide_rules_or_escape_policy_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    okf::json(
        okf::learn(root)
            .args([
                "create",
                "global-visible",
                "--global",
                "--file-glob",
                "src/*.rs",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    okf::json(
        okf::learn(root)
            .args(["create", "repo-omit", "--file-glob", "src/*.rs", "--json"])
            .output()
            .unwrap(),
    );
    let outside = finish(start(root, root, "outside-git", "Edit"));
    assert!(outside.contains("global-visible"));
    assert!(!outside.contains("repo-omit"));
    let path = root.join("policy/.sessions");
    for file in std::fs::read_dir(&path).unwrap() {
        let file = file.unwrap().path();
        if file.extension().is_some_and(|s| s == "json") {
            std::fs::write(file, "corrupt").unwrap();
        }
    }
    assert!(finish(start(root, root, "outside-git", "Edit")).contains("global-visible"));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join(".writer.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let began = std::time::Instant::now();
    let while_locked = finish(start(root, root, "paused-writer", "Edit"));
    assert!(while_locked.contains("global-visible"));
    assert!(
        began.elapsed() < std::time::Duration::from_secs(5),
        "optional cache lock blocked hook indefinitely"
    );
    fs2::FileExt::unlock(&lock).unwrap();
    let backup = root.join("policy-backup");
    KnowledgeStore::open(&root.join("policy"), false)
        .unwrap()
        .backup(&backup)
        .unwrap();
    assert!(!backup.join("corpus/.sessions").exists());
    std::fs::remove_dir_all(&path).unwrap();
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("sentinel"), "unchanged").unwrap();
    symlink(&elsewhere, &path).unwrap();
    let emitted = finish(start(root, root, "cannot-follow", "Edit"));
    assert!(emitted.contains("global-visible"));
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 1);
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("sentinel")).unwrap(),
        "unchanged"
    );
    let began = std::time::Instant::now();
    let out = start_using(
        root,
        root,
        "database-outage",
        "Edit",
        Some("postgres://private-user:secret-hook-password@127.0.0.1:1/absent"),
    )
    .wait_with_output()
    .unwrap();
    assert!(out.status.success());
    assert!(began.elapsed() < std::time::Duration::from_secs(10));
    assert!(String::from_utf8_lossy(&out.stdout).contains("global-visible"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("secret-hook-password"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("secret-hook-password"));
    binding.phase = ygg::knowledge::runtime::Phase::Fenced;
    okf::select(root, &binding);
    assert!(finish(start(root, root, "fenced", "Edit")).is_empty());
}

#[tokio::test]
async fn local_rule_injection_preserves_database_locks_without_sql_knowledge_fallback() {
    let database = std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    okf::json(
        okf::learn(root)
            .args([
                "create",
                "local-healthy-visible",
                "--file-glob",
                "src/*.rs",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let pool = ygg::db::create_pool(&database).await.unwrap();
    let agent = format!("hook-fixture-{}", Uuid::new_v4());
    let file = format!("src/{}.rs", Uuid::new_v4());
    let sentinel = format!("frozen-rule-{}", Uuid::new_v4());
    let rule: Uuid = sqlx::query_scalar("INSERT INTO learnings(text,file_glob,user_id) VALUES($1,'src/*.rs','legacy-user') RETURNING learning_id")
        .bind(&sentinel).fetch_one(&pool).await.unwrap();
    let invoke = || {
        let mut child = okf::app(root, &root.join("repo"))
            .env("DATABASE_URL", &database)
            .env("YGG_AGENT_NAME", &agent)
            .args(["hook", "pre-tool-use"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                serde_json::to_string(&serde_json::json!({
                    "tool_name":"Edit", "tool_input":{"file_path":file}, "session_id":agent
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let output = invoke();
    let held: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM locks l JOIN agents a ON a.agent_id=l.agent_id WHERE l.resource_key=$1 AND a.agent_name=$2 AND a.user_id='legacy-user')")
        .bind(&file).bind(&agent).fetch_one(&pool).await.unwrap();
    binding.phase = ygg::knowledge::runtime::Phase::Fenced;
    okf::select(root, &binding);
    let fenced = invoke();
    sqlx::query("DELETE FROM locks WHERE resource_key=$1 AND agent_id IN (SELECT agent_id FROM agents WHERE agent_name=$2 AND user_id='legacy-user')")
        .bind(&file).bind(&agent).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM learnings WHERE learning_id=$1")
        .bind(rule)
        .execute(&pool)
        .await
        .unwrap();
    let agent_id: Uuid = sqlx::query_scalar(
        "SELECT agent_id FROM agents WHERE agent_name=$1 AND user_id='legacy-user'",
    )
    .bind(&agent)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM events WHERE agent_id=$1 OR session_id IN (SELECT session_id FROM sessions WHERE agent_id=$1)")
        .bind(agent_id).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM sessions WHERE agent_id=$1")
        .bind(agent_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM agents WHERE agent_name=$1 AND user_id='legacy-user'")
        .bind(&agent)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(
        held,
        "offline dispatch bypassed healthy shared lock acquisition"
    );
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("local-healthy-visible")
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&sentinel));
    assert!(fenced.status.success());
    assert!(fenced.stdout.is_empty(), "fenced selection used SQL rules");
}
