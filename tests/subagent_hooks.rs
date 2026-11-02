//! ADR 0020: native Claude Code subagents are tracked through the
//! `SubagentStart` / `SubagentStop` hooks as children of the parent agent.
//! Payload shapes mirror Claude Code 2.1.310 (captured from real hook runs).
//!
//! Requires an isolated Postgres with migrations applied:
//!     DATABASE_URL=postgres://... cargo test --test subagent_hooks -- --test-threads=1

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use uuid::Uuid;

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    user: String,
    session: String,
    db_url: String,
}

impl Fixture {
    fn new() -> Self {
        let db_url = std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let tag = &Uuid::new_v4().simple().to_string()[..8];
        let repo = temp.path().join(format!("parent-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        Self {
            _temp: temp,
            home,
            repo,
            user: format!("subagent-test-{tag}"),
            session: Uuid::new_v4().to_string(),
            db_url,
        }
    }

    fn parent_name(&self) -> String {
        self.repo.file_name().unwrap().to_string_lossy().to_string()
    }

    fn ygg(&self, cwd: &Path, db_url: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ygg"));
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("YGG_CONFIG_DIR", self.home.join("config"))
            .env("YGG_DATA_DIR", self.home.join("data"))
            .env("YGG_USER", &self.user)
            .env("YGG_STOP_WATCHER_TICK", "0")
            .env("YGG_RUN_CAPTURE", "0")
            .env("DATABASE_URL", db_url)
            .current_dir(cwd);
        cmd
    }

    fn hook_in(&self, cwd: &Path, event: &str, mut payload: serde_json::Value) -> Output {
        self.hook_with_db(cwd, event, &mut payload, &self.db_url.clone())
    }

    fn hook_with_db(
        &self,
        cwd: &Path,
        event: &str,
        payload: &mut serde_json::Value,
        db_url: &str,
    ) -> Output {
        let obj = payload.as_object_mut().unwrap();
        obj.entry("session_id")
            .or_insert_with(|| self.session.clone().into());
        obj.entry("cwd")
            .or_insert_with(|| cwd.to_string_lossy().to_string().into());
        let mut child = self
            .ygg(cwd, db_url)
            .args(["hook", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "hook {event} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn hook(&self, event: &str, payload: serde_json::Value) -> Output {
        let repo = self.repo.clone();
        self.hook_in(&repo, event, payload)
    }

    async fn pool(&self) -> sqlx::PgPool {
        ygg::db::create_pool(&self.db_url).await.unwrap()
    }

    async fn rows(
        &self,
    ) -> Vec<(
        String,
        String,
        Option<String>,
        String,
        bool,
        Option<String>,
        i64,
    )> {
        sqlx::query_as(
            "SELECT a.agent_name, s.agent_type, s.description, s.state, s.background,
                    s.last_tool, s.output_tokens
               FROM subagents s JOIN agents a ON a.agent_id = s.parent_agent_id
              WHERE s.user_id = $1 AND s.state <> 'pending'
              ORDER BY s.started_at",
        )
        .bind(&self.user)
        .fetch_all(&self.pool().await)
        .await
        .unwrap()
    }
}

fn agent_call(tool_use_id: &str, kind: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Agent",
        "tool_use_id": tool_use_id,
        "tool_input": {
            "description": description,
            "prompt": "do the thing",
            "subagent_type": kind,
            "run_in_background": true,
            "isolation": "worktree"
        }
    })
}

fn start(agent_id: &str, kind: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "SubagentStart",
        "agent_id": agent_id,
        "agent_type": kind
    })
}

fn stop(agent_id: &str, kind: &str, transcript: Option<&Path>) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "SubagentStop",
        "stop_hook_active": false,
        "agent_id": agent_id,
        "agent_type": kind,
        "agent_transcript_path": transcript.map(|p| p.to_string_lossy().to_string()),
        "last_assistant_message": "Found 3 issues."
    })
}

fn subagent_tool(agent_id: &str, tool: &str, input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "agent_id": agent_id,
        "agent_type": "Explore",
        "tool_name": tool,
        "tool_input": input,
        "tool_use_id": format!("toolu_{}", Uuid::new_v4().simple())
    })
}

#[tokio::test]
async fn subagent_lifecycle_is_tracked_under_parent() {
    let fx = Fixture::new();
    let transcript = fx.home.join("agent-a1.jsonl");
    std::fs::write(
        &transcript,
        [
            r#"{"uuid":"u1","message":{"role":"user"}}"#,
            r#"{"uuid":"u2","message":{"role":"assistant","usage":{"input_tokens":120,"output_tokens":45}}}"#,
            r#"{"uuid":"u3","message":{"role":"assistant","usage":{"input_tokens":30,"output_tokens":5}}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let outputs = [
        fx.hook(
            "pre-tool-use",
            agent_call("toolu_a", "Explore", "map the hooks"),
        ),
        fx.hook("subagent-start", start("a1", "Explore")),
        fx.hook(
            "pre-tool-use",
            subagent_tool("a1", "Bash", serde_json::json!({"command": "ls"})),
        ),
    ];
    let rows = fx.rows().await;
    assert_eq!(rows.len(), 1);
    let (parent, kind, desc, state, background, last_tool, _) = &rows[0];
    assert_eq!(parent, &fx.parent_name());
    assert_eq!(kind, "Explore");
    assert_eq!(desc.as_deref(), Some("map the hooks"));
    assert_eq!(state, "running");
    assert!(background);
    assert_eq!(last_tool.as_deref(), Some("Bash"));

    let status = fx.ygg(&fx.repo, &fx.db_url).arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        text.contains("running  Explore: map the hooks"),
        "status missing subagent: {text}"
    );

    let stop_out = fx.hook("subagent-stop", stop("a1", "Explore", Some(&transcript)));
    let rows = fx.rows().await;
    assert_eq!(rows[0].3, "done");
    assert_eq!(rows[0].6, 50, "output tokens from the subagent transcript");

    // Subagent hooks never write to stdout: SubagentStop JSON would be read
    // as a decision that keeps the subagent running.
    for out in outputs.iter().chain([&stop_out]) {
        assert!(
            out.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[tokio::test]
async fn subagent_hooks_are_idempotent_and_resumable() {
    let fx = Fixture::new();
    fx.hook("pre-tool-use", agent_call("toolu_b", "Plan", "plan it"));
    fx.hook("pre-tool-use", agent_call("toolu_b", "Plan", "plan it"));
    fx.hook("subagent-start", start("b1", "Plan"));
    fx.hook("subagent-start", start("b1", "Plan"));
    fx.hook("subagent-stop", stop("b1", "Plan", None));
    fx.hook("subagent-stop", stop("b1", "Plan", None));
    let rows = fx.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].2.as_deref(), Some("plan it"));
    assert_eq!(rows[0].3, "done");

    // A resumed subagent fires SubagentStart again for the same id.
    fx.hook("subagent-start", start("b1", "Plan"));
    let rows = fx.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].3, "running");
}

#[tokio::test]
async fn descriptions_match_calls_by_type_in_order() {
    let fx = Fixture::new();
    fx.hook(
        "pre-tool-use",
        agent_call("toolu_1", "Explore", "first explore"),
    );
    fx.hook("pre-tool-use", agent_call("toolu_2", "Plan", "the plan"));
    fx.hook(
        "pre-tool-use",
        agent_call("toolu_3", "Explore", "second explore"),
    );
    fx.hook("subagent-start", start("p1", "Plan"));
    fx.hook("subagent-start", start("e1", "Explore"));
    fx.hook("subagent-start", start("e2", "Explore"));
    let mut descs: Vec<(String, Option<String>)> =
        fx.rows().await.into_iter().map(|r| (r.1, r.2)).collect();
    descs.sort();
    assert_eq!(
        descs,
        vec![
            ("Explore".into(), Some("first explore".into())),
            ("Explore".into(), Some("second explore".into())),
            ("Plan".into(), Some("the plan".into())),
        ]
    );
}

#[tokio::test]
async fn internal_agent_stop_without_start_is_ignored() {
    let fx = Fixture::new();
    // Prompt suggestions and /btw fire SubagentStop with an empty agent_type.
    fx.hook("subagent-stop", stop("internal", "", None));
    assert!(fx.rows().await.is_empty());
    // A real subagent whose start was missed is still recorded on stop.
    fx.hook("subagent-stop", stop("late", "Explore", None));
    assert_eq!(fx.rows().await.len(), 1);
}

#[tokio::test]
async fn worktree_subagent_is_attributed_to_parent_repo() {
    let fx = Fixture::new();
    let worktree = fx.repo.join(".claude/worktrees/agent-w1");
    std::fs::create_dir_all(&worktree).unwrap();
    fx.hook(
        "pre-tool-use",
        agent_call("toolu_w", "general-purpose", "isolated"),
    );
    fx.hook_in(&worktree, "subagent-start", start("w1", "general-purpose"));
    fx.hook_in(
        &worktree,
        "pre-tool-use",
        subagent_tool("w1", "Bash", serde_json::json!({"command": "pwd"})),
    );
    fx.hook_in(
        &worktree,
        "subagent-stop",
        stop("w1", "general-purpose", None),
    );

    let rows = fx.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, fx.parent_name());
    assert_eq!(rows[0].5.as_deref(), Some("Bash"));
    let strays: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agents WHERE user_id = $1 AND agent_name = 'agent-w1'",
    )
    .bind(&fx.user)
    .fetch_one(&fx.pool().await)
    .await
    .unwrap();
    assert_eq!(strays, 0, "subagent worktree minted its own agent identity");
}

#[tokio::test]
async fn parent_stop_keeps_locks_while_a_subagent_runs() {
    let fx = Fixture::new();
    let file = fx.repo.join("src/lib.rs").to_string_lossy().to_string();
    fx.hook(
        "pre-tool-use",
        agent_call("toolu_l", "general-purpose", "edit lib"),
    );
    fx.hook("subagent-start", start("l1", "general-purpose"));
    // The subagent's edit takes the lock under the parent agent's identity.
    fx.hook(
        "pre-tool-use",
        subagent_tool("l1", "Edit", serde_json::json!({"file_path": file})),
    );
    let pool = fx.pool().await;
    let lock_owner = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT a.agent_name FROM locks l JOIN agents a ON a.agent_id = l.agent_id
              WHERE l.resource_key = $1",
        )
        .bind(&file)
        .fetch_optional(&pool)
        .await
        .unwrap()
    };
    assert_eq!(lock_owner().await, Some(fx.parent_name()));

    // Claude Code fires the parent's Stop while a background subagent works.
    let out = fx.hook("stop", serde_json::json!({"hook_event_name": "Stop"}));
    assert!(String::from_utf8_lossy(&out.stderr).contains("subagent(s) still running"));
    assert_eq!(lock_owner().await, Some(fx.parent_name()));

    fx.hook("subagent-stop", stop("l1", "general-purpose", None));
    fx.hook("stop", serde_json::json!({"hook_event_name": "Stop"}));
    assert_eq!(
        lock_owner().await,
        None,
        "locks released once subagents end"
    );
}

#[test]
fn subagent_hooks_fail_open_without_database() {
    let fx = Fixture::new();
    let dead = "postgres://nobody:never-print-this@127.0.0.1:1/absent";
    let started = std::time::Instant::now();
    for (event, mut payload) in [
        ("subagent-start", start("x1", "Explore")),
        ("subagent-stop", stop("x1", "Explore", None)),
        ("pre-tool-use", agent_call("toolu_x", "Explore", "offline")),
    ] {
        let out = fx.hook_with_db(&fx.repo, event, &mut payload, dead);
        assert!(out.stdout.is_empty(), "{event} wrote to stdout");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("never-print-this"));
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

#[test]
fn prime_teaches_native_agents_for_in_session_parallelism() {
    let fx = Fixture::new();
    let out = fx.ygg(&fx.repo, &fx.db_url).arg("prime").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("native Agent tool"), "{text}");
    assert!(text.contains("only for detached work"), "{text}");
    assert!(
        !text.contains("instead of the native Task/Agent tool"),
        "{text}"
    );
}
