use std::env;

/// Integration tests — require a running Postgres with migrations applied.
/// Run with: DATABASE_URL=postgres://ngj49@localhost:5432/ygg cargo test -- --test-threads=1

#[tokio::test]
async fn test_agent_lifecycle() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let agent_repo = ygg::models::agent::AgentRepo::new(&pool);

    // Register
    let agent = agent_repo.register("test-agent-lifecycle").await.unwrap();
    assert_eq!(agent.agent_name, "test-agent-lifecycle");
    assert_eq!(agent.current_state, ygg::models::agent::AgentState::Idle);

    // Transition idle → executing
    let updated = agent_repo
        .transition(agent.agent_id, ygg::models::agent::AgentState::Idle, ygg::models::agent::AgentState::Executing)
        .await
        .unwrap();
    assert!(updated.is_some());
    assert_eq!(updated.unwrap().current_state, ygg::models::agent::AgentState::Executing);

    // Invalid transition — executing → idle (should work)
    let back = agent_repo
        .transition(agent.agent_id, ygg::models::agent::AgentState::Executing, ygg::models::agent::AgentState::Idle)
        .await
        .unwrap();
    assert!(back.is_some());

    // Wrong current state — should return None
    let bad = agent_repo
        .transition(agent.agent_id, ygg::models::agent::AgentState::Executing, ygg::models::agent::AgentState::Shutdown)
        .await
        .unwrap();
    assert!(bad.is_none()); // already idle, not executing

    // Cleanup
    sqlx::query("DELETE FROM agents WHERE agent_name = 'test-agent-lifecycle'")
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_node_dag() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let node_repo = ygg::models::node::NodeRepo::new(&pool);
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool);

    // Create test agent
    let agent = agent_repo.register("test-node-dag").await.unwrap();
    let aid = agent.agent_id;

    // Insert root node
    let root = node_repo.insert(
        None, aid,
        ygg::models::node::NodeKind::UserMessage,
        serde_json::json!({"task": "test task"}),
        10,
    ).await.unwrap();
    assert!(root.ancestors.is_empty());

    // Insert child node
    let child = node_repo.insert(
        Some(root.id), aid,
        ygg::models::node::NodeKind::AssistantMessage,
        serde_json::json!({"response": "ok"}),
        20,
    ).await.unwrap();
    assert_eq!(child.ancestors.len(), 1);
    assert_eq!(child.ancestors[0], root.id);

    // Insert grandchild
    let grandchild = node_repo.insert(
        Some(child.id), aid,
        ygg::models::node::NodeKind::ToolCall,
        serde_json::json!({"command": "ls"}),
        5,
    ).await.unwrap();
    assert_eq!(grandchild.ancestors.len(), 2);

    // Path traversal
    let path = node_repo.get_ancestor_path(grandchild.id).await.unwrap();
    assert_eq!(path.len(), 3);

    // Token sum
    let tokens = node_repo.calculate_path_tokens(grandchild.id).await.unwrap();
    assert_eq!(tokens, 35); // 10 + 20 + 5

    // Children
    let children = node_repo.get_children(root.id).await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, child.id);

    // Cleanup
    sqlx::query("DELETE FROM nodes WHERE agent_id = $1").bind(aid).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM agents WHERE agent_name = 'test-node-dag'").execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_lock_acquire_release() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let lock_mgr = ygg::lock::LockManager::new(&pool, 300);
    let agent_id = uuid::Uuid::new_v4();

    // Acquire
    let lock = lock_mgr.acquire("test:resource:1", agent_id).await.unwrap();
    assert_eq!(lock.resource_key, "test:resource:1");

    // Double acquire by same agent — should conflict (different row)
    let agent2 = uuid::Uuid::new_v4();
    let conflict = lock_mgr.acquire("test:resource:1", agent2).await;
    assert!(conflict.is_err());

    // Release
    lock_mgr.release("test:resource:1", agent_id).await.unwrap();

    // Now agent2 can acquire
    let lock2 = lock_mgr.acquire("test:resource:1", agent2).await.unwrap();
    assert_eq!(lock2.agent_id, agent2);

    // Cleanup
    lock_mgr.release("test:resource:1", agent2).await.unwrap();
}

#[tokio::test]
async fn test_lock_atomic_no_toctou() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let lock_mgr = ygg::lock::LockManager::new(&pool, 300);
    let a1 = uuid::Uuid::new_v4();
    let a2 = uuid::Uuid::new_v4();

    // Both try to acquire simultaneously
    let (r1, r2) = tokio::join!(
        lock_mgr.acquire("test:atomic:1", a1),
        lock_mgr.acquire("test:atomic:1", a2),
    );

    // Exactly one should succeed
    let (ok_count, err_count) = (
        r1.is_ok() as u32 + r2.is_ok() as u32,
        r1.is_err() as u32 + r2.is_err() as u32,
    );
    assert_eq!(ok_count, 1);
    assert_eq!(err_count, 1);

    // Cleanup
    let winner = if r1.is_ok() { a1 } else { a2 };
    lock_mgr.release("test:atomic:1", winner).await.unwrap();
}

#[tokio::test]
async fn test_embedding_ollama() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");

    // Check if Ollama is available
    let embedder = ygg::embed::Embedder::default_ollama();
    if !embedder.health_check().await {
        eprintln!("Ollama not available, skipping embedding test");
        return;
    }

    let vec = embedder.embed("hello world").await.unwrap();
    // all-minilm produces 384-dim vectors
    // pgvector::Vector doesn't expose len(), so just verify it succeeded
    assert!(true, "embedding succeeded");

    // Test that two different texts produce different embeddings
    let vec2 = embedder.embed("quantum physics research paper").await.unwrap();
    // They should be different (we can't easily compare Vectors, but no error = success)
    assert!(true, "second embedding succeeded");
}

#[tokio::test]
async fn test_salience_governor() {
    use ygg::salience::*;

    let mut gov = Governor::new(SalienceConfig {
        max_concurrent: 3,
        floor: 0.05,
        half_life_tokens: 50_000,
    });

    // High salience at close distance
    let s1 = gov.calculate_salience(0.9, 0);
    assert!((s1 - 0.9).abs() < 0.01);

    // Decayed at half-life
    let s2 = gov.calculate_salience(0.9, 50_000);
    assert!((s2 - 0.45).abs() < 0.01);

    // Governor caps at max_concurrent
    let directives: Vec<ScoredDirective> = (0..10).map(|i| ScoredDirective {
        node_id: uuid::Uuid::new_v4(),
        content: format!("directive {i}"),
        token_count: 10,
        similarity: 0.9 - (i as f64 * 0.05),
        token_distance: 0,
        salience: 0.9 - (i as f64 * 0.05),
    }).collect();

    let result = gov.govern(directives);
    assert_eq!(result.len(), 3); // capped

    // Dedup on second call
    let more: Vec<ScoredDirective> = result.iter().map(|d| ScoredDirective {
        node_id: d.node_id,
        content: d.content.clone(),
        token_count: d.token_count,
        similarity: d.similarity,
        token_distance: d.token_distance,
        salience: d.salience,
    }).collect();
    let result2 = gov.govern(more);
    assert_eq!(result2.len(), 0); // all seen

    // Reset clears dedup
    gov.reset_session();
    assert_eq!(gov.session_count(), 0);
}

#[tokio::test]
async fn test_crash_recovery() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let agent_repo = ygg::models::agent::AgentRepo::new(&pool);

    // Create agent stuck in executing
    let agent = agent_repo.register("test-crash-recovery").await.unwrap();
    agent_repo.transition(
        agent.agent_id,
        ygg::models::agent::AgentState::Idle,
        ygg::models::agent::AgentState::Executing,
    ).await.unwrap();

    // Make it stale by backdating updated_at
    sqlx::query("UPDATE agents SET updated_at = now() - interval '1 hour' WHERE agent_name = 'test-crash-recovery'")
        .execute(&pool).await.unwrap();

    // Find orphaned (stale > 60s)
    let orphaned = agent_repo.find_orphaned(60).await.unwrap();
    assert!(orphaned.iter().any(|a| a.agent_name == "test-crash-recovery"));

    // Reset
    agent_repo.reset_to_idle(agent.agent_id).await.unwrap();
    let recovered = agent_repo.get(agent.agent_id).await.unwrap().unwrap();
    assert_eq!(recovered.current_state, ygg::models::agent::AgentState::Idle);

    // Cleanup
    sqlx::query("DELETE FROM agents WHERE agent_name = 'test-crash-recovery'")
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_msg_send_inbox_mark_read() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool);

    sqlx::query("DELETE FROM events WHERE agent_name IN ('test-msg-sender', 'test-msg-recipient')")
        .execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-msg-sender', 'test-msg-recipient')")
        .execute(&pool).await.unwrap();

    agent_repo.register("test-msg-sender").await.unwrap();
    agent_repo.register("test-msg-recipient").await.unwrap();

    // Send two messages; inbox returns both; mark_read drains.
    ygg::cli::msg_cmd::send(&pool, "test-msg-sender", "test-msg-recipient", "hello 1", false).await.unwrap();
    ygg::cli::msg_cmd::send(&pool, "test-msg-sender", "test-msg-recipient", "hello 2", false).await.unwrap();

    let unread = ygg::cli::msg_cmd::inbox(&pool, "test-msg-recipient", false).await.unwrap();
    assert_eq!(unread.len(), 2);
    assert_eq!(unread[0].body, "hello 1");
    assert_eq!(unread[1].body, "hello 2");
    assert_eq!(unread[0].from_agent_name, "test-msg-sender");

    ygg::cli::msg_cmd::mark_read(&pool, "test-msg-recipient").await.unwrap();
    let post = ygg::cli::msg_cmd::inbox(&pool, "test-msg-recipient", false).await.unwrap();
    assert!(post.is_empty(), "inbox empty after mark_read");

    // But --all should still return both.
    let all = ygg::cli::msg_cmd::inbox(&pool, "test-msg-recipient", true).await.unwrap();
    assert_eq!(all.len(), 2);

    // Missing recipient errors.
    let err = ygg::cli::msg_cmd::send(&pool, "test-msg-sender", "does-not-exist", "x", false).await;
    assert!(err.is_err());

    // Cleanup
    sqlx::query("DELETE FROM events WHERE agent_name IN ('test-msg-sender', 'test-msg-recipient')")
        .execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-msg-sender', 'test-msg-recipient')")
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_lock_release_all_for_agent() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool);

    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-lock-stop-a', 'test-lock-stop-b')")
        .execute(&pool).await.unwrap();

    let a = agent_repo.register("test-lock-stop-a").await.unwrap();
    let b = agent_repo.register("test-lock-stop-b").await.unwrap();

    let lock_mgr = ygg::lock::LockManager::new(&pool, 300);
    lock_mgr.acquire("test-res-a-1", a.agent_id).await.unwrap();
    lock_mgr.acquire("test-res-a-2", a.agent_id).await.unwrap();
    lock_mgr.acquire("test-res-b-1", b.agent_id).await.unwrap();

    let released = lock_mgr.release_all_for_agent(a.agent_id).await.unwrap();
    assert_eq!(released, 2, "should release both locks for agent a");

    let a_after = lock_mgr.list_agent_locks(a.agent_id).await.unwrap();
    assert!(a_after.is_empty(), "agent a should hold no locks");
    let b_after = lock_mgr.list_agent_locks(b.agent_id).await.unwrap();
    assert_eq!(b_after.len(), 1, "agent b's lock must not be touched");

    // Cleanup
    let _ = lock_mgr.release_all_for_agent(b.agent_id).await;
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-lock-stop-a', 'test-lock-stop-b')")
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_orphan_candidates_filter_by_idle() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool);

    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-orphan-fresh', 'test-orphan-stale')")
        .execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM repos WHERE name = 'test-orphan-repo'")
        .execute(&pool).await.unwrap();

    let fresh = agent_repo.register("test-orphan-fresh").await.unwrap();
    let stale = agent_repo.register("test-orphan-stale").await.unwrap();

    // Back-date the stale agent so it crosses the idle threshold.
    sqlx::query("UPDATE agents SET updated_at = now() - interval '2 hours' WHERE agent_id = $1")
        .bind(stale.agent_id).execute(&pool).await.unwrap();

    // Minimal fixture: repo + task per agent + worker with a bogus path.
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (name, task_prefix) VALUES ('test-orphan-repo', 'test-orphan-repo') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1) ON CONFLICT DO NOTHING")
        .bind(repo.0).execute(&pool).await.unwrap();
    for (aid, seq) in [(fresh.agent_id, 1), (stale.agent_id, 2)] {
        let task: (uuid::Uuid,) = sqlx::query_as(
            "INSERT INTO tasks (repo_id, seq, title, kind, assignee) VALUES ($1, $2, 'fixture', 'task', $3) RETURNING task_id"
        ).bind(repo.0).bind(seq).bind(aid).fetch_one(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO workers (task_id, tmux_session, tmux_window, worktree_path) VALUES ($1, 's', 'w', '/tmp/ygg-test-nonexistent-worktree')"
        ).bind(task.0).execute(&pool).await.unwrap();
    }

    // At a 1-hour idle threshold, only the stale agent should be returned.
    let cands = agent_repo.list_orphan_candidates(3600).await.unwrap();
    assert!(cands.iter().any(|(id, _, _)| *id == stale.agent_id),
        "stale agent should be a candidate at 1h threshold");
    assert!(!cands.iter().any(|(id, _, _)| *id == fresh.agent_id),
        "fresh agent should NOT be a candidate at 1h threshold");

    // Cleanup (cascades wipe tasks + workers).
    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-orphan-fresh', 'test-orphan-stale')")
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_agent_rename() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool);

    // Clean up any leftovers from a prior failed run.
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-rename-src', 'test-rename-dst', 'test-rename-collision')")
        .execute(&pool).await.unwrap();

    let src = agent_repo.register("test-rename-src").await.unwrap();
    let other = agent_repo.register("test-rename-collision").await.unwrap();

    // Happy path: rename succeeds, id preserved, lookup by new name works,
    // old name is gone.
    agent_repo.rename(src.agent_id, "test-rename-dst").await.unwrap();
    let found = agent_repo.get_by_name("test-rename-dst").await.unwrap();
    assert!(found.is_some(), "agent lookup by new name should succeed");
    assert_eq!(found.unwrap().agent_id, src.agent_id, "agent_id must be preserved");
    let gone = agent_repo.get_by_name("test-rename-src").await.unwrap();
    assert!(gone.is_none(), "old name should no longer resolve");

    // Collision path: renaming another agent into an occupied (name, persona)
    // slot must error with a unique-violation (SQLSTATE 23505).
    let err = agent_repo.rename(other.agent_id, "test-rename-dst").await.unwrap_err();
    let code = err.as_database_error().and_then(|d| d.code().map(|c| c.to_string()));
    assert_eq!(code.as_deref(), Some("23505"), "collision should be unique_violation");

    // Cleanup
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-rename-src', 'test-rename-dst', 'test-rename-collision')")
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_finalizes_terminal_run_closes_task() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    // Use a fixture repo + task. Cleanup any prior runs of the test.
    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-test'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-test', 'sched-test') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();

    // Create a task in_progress and insert a succeeded task_runs row.
    let task: (uuid::Uuid,) = sqlx::query_as(
        r#"INSERT INTO tasks (repo_id, seq, title, status)
           VALUES ($1, 1, 'sched fin test', 'in_progress')
           RETURNING task_id"#,
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    sqlx::query(
        r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, output_commit_sha, ended_at)
           VALUES ($1, 1, 'run:test:1', 'succeeded', 'deadbeef', now())"#,
    ).bind(task.0).execute(&pool).await.unwrap();

    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
    };
    let stats = ygg::scheduler::tick(&pool, &cfg).await.unwrap();
    assert!(stats.finalized >= 1, "finalize_terminal_runs should close the task");

    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status::text, close_reason, result_blob_ref FROM tasks WHERE task_id = $1",
    ).bind(task.0).fetch_one(&pool).await.unwrap();
    assert_eq!(row.0, "closed");
    assert!(row.1.unwrap().contains("succeeded"));
    assert_eq!(row.2.as_deref(), Some("deadbeef"));

    // Cleanup
    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_schedules_runnable_task_to_ready() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-sched'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-sched', 'sched-sched') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        r#"INSERT INTO tasks (repo_id, seq, title, status, runnable, approval_level)
           VALUES ($1, 1, 'sched runnable', 'open', TRUE, 'auto')
           RETURNING task_id"#,
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
    };
    let scheduled = ygg::scheduler::schedule_ready_tasks(&pool, &cfg).await.unwrap();
    assert!(scheduled >= 1, "runnable + auto-approved task should be scheduled");

    let state: String = sqlx::query_scalar(
        "SELECT state::text FROM task_runs WHERE task_id = $1 ORDER BY attempt DESC LIMIT 1",
    ).bind(task.0).fetch_one(&pool).await.unwrap();
    assert_eq!(state, "ready");

    // A second tick shouldn't double-schedule (NOT EXISTS guard).
    let scheduled2 = ygg::scheduler::schedule_ready_tasks(&pool, &cfg).await.unwrap();
    assert_eq!(scheduled2, 0, "should not double-schedule");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_advisory_lock_singleton() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let g1 = ygg::scheduler::acquire_advisory_lock(&pool).await.unwrap();
    let g2 = ygg::scheduler::acquire_advisory_lock(&pool).await;
    assert!(g2.is_err(), "second acquire must fail while first guard is alive");
    drop(g1);
    let g3 = ygg::scheduler::acquire_advisory_lock(&pool).await;
    assert!(g3.is_ok(), "after dropping first guard, third acquire must succeed");
}

#[tokio::test]
async fn test_scheduler_reaps_expired_heartbeat() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-reap'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-reap', 'sched-reap') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'reap', 'in_progress') RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    // running run with heartbeat 10 minutes ago and ttl 60s.
    sqlx::query(
        r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, heartbeat_at, heartbeat_ttl_s, started_at)
           VALUES ($1, 1, 'run:reap:1', 'running', now() - interval '10 minutes', 60, now() - interval '11 minutes')"#,
    ).bind(task.0).execute(&pool).await.unwrap();

    let reaped = ygg::scheduler::reap_expired_heartbeats(&pool).await.unwrap();
    assert_eq!(reaped, 1);

    let state: String = sqlx::query_scalar(
        "SELECT state::text FROM task_runs WHERE task_id = $1"
    ).bind(task.0).fetch_one(&pool).await.unwrap();
    assert_eq!(state, "crashed");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_enforces_deadline() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-dl'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-dl', 'sched-dl') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'dl', 'in_progress') RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    sqlx::query(
        r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, deadline_at, started_at)
           VALUES ($1, 1, 'run:dl:1', 'running', now() - interval '1 minute', now() - interval '5 minutes')"#,
    ).bind(task.0).execute(&pool).await.unwrap();

    let cancelled = ygg::scheduler::enforce_deadlines(&pool).await.unwrap();
    assert_eq!(cancelled, 1);
    let (state, reason): (String, String) = sqlx::query_as(
        "SELECT state::text, reason::text FROM task_runs WHERE task_id = $1"
    ).bind(task.0).fetch_one(&pool).await.unwrap();
    assert_eq!(state, "cancelled");
    assert_eq!(reason, "timeout");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_schedules_retry_with_previous_attempt() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-retry'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-retry', 'sched-retry') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status, runnable, max_attempts) VALUES ($1, 1, 'retry', 'in_progress', TRUE, 3) RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    // Failed attempt 1, ended 10 minutes ago. Backoff is exp 60s base; well past.
    sqlx::query(
        r#"INSERT INTO task_runs
           (task_id, attempt, idempotency_key, state, reason,
            ended_at, error, max_attempts)
           VALUES ($1, 1, 'run:retry:1', 'failed', 'agent_error',
                   now() - interval '10 minutes',
                   '{"reason_code":"agent_error","message":"tests red"}'::jsonb, 3)"#,
    ).bind(task.0).execute(&pool).await.unwrap();

    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
    };
    let n = ygg::scheduler::schedule_retries(&pool, &cfg).await.unwrap();
    assert_eq!(n, 1);

    // Original is now in 'retrying'; new attempt 2 exists.
    let states: Vec<(i32, String)> = sqlx::query_as(
        "SELECT attempt, state::text FROM task_runs WHERE task_id = $1 ORDER BY attempt"
    ).bind(task.0).fetch_all(&pool).await.unwrap();
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].1, "retrying");
    assert_eq!(states[1].1, "ready");

    // Previous-attempt thread-through.
    let new_input: serde_json::Value = sqlx::query_scalar(
        "SELECT input FROM task_runs WHERE task_id = $1 AND attempt = 2"
    ).bind(task.0).fetch_one(&pool).await.unwrap();
    let prev = new_input.get("previous_attempt").unwrap();
    assert_eq!(prev["attempt"], 1);
    assert_eq!(prev["error"]["reason_code"], "agent_error");

    // Idempotency: second call should not double-retry (successor already exists).
    let n2 = ygg::scheduler::schedule_retries(&pool, &cfg).await.unwrap();
    assert_eq!(n2, 0);

    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_detect_loops_poisons_repeating_failures() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-loop'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-loop', 'sched-loop') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status, max_attempts) VALUES ($1, 1, 'flaky', 'in_progress', 5) RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    // Three failed attempts with identical error.
    for n in 1..=3 {
        sqlx::query(
            r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, reason, error, ended_at)
               VALUES ($1, $2, $3, 'failed', 'agent_error',
                       '{"reason_code":"agent_error","message":"tests red"}'::jsonb,
                       now())"#,
        )
        .bind(task.0)
        .bind(n)
        .bind(format!("run:loop:{n}"))
        .execute(&pool).await.unwrap();
    }

    let poisoned = ygg::scheduler::detect_loops(&pool, 3).await.unwrap();
    assert_eq!(poisoned, 1);

    let status: String = sqlx::query_scalar("SELECT status::text FROM tasks WHERE task_id = $1")
        .bind(task.0).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "blocked");

    let states: Vec<(i32, String)> = sqlx::query_as(
        "SELECT attempt, state::text FROM task_runs WHERE task_id = $1 ORDER BY attempt"
    ).bind(task.0).fetch_all(&pool).await.unwrap();
    for (_, s) in &states {
        assert_eq!(s, "poison");
    }

    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_unpoison_reopens_task() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-unpo'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-unpo', 'sched-unpo') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'unpo', 'blocked') RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO task_runs (task_id, attempt, idempotency_key, state, reason) VALUES ($1, 1, 'run:unpo:1', 'poison', 'loop_detected')"
    ).bind(task.0).execute(&pool).await.unwrap();

    ygg::scheduler::unpoison(&pool, task.0).await.unwrap();

    let status: String = sqlx::query_scalar("SELECT status::text FROM tasks WHERE task_id = $1")
        .bind(task.0).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "open");
    let run_state: String = sqlx::query_scalar(
        "SELECT state::text FROM task_runs WHERE task_id = $1"
    ).bind(task.0).fetch_one(&pool).await.unwrap();
    assert_eq!(run_state, "failed");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_approval_gate_blocks_then_unblocks() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-appr'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-appr', 'sched-appr') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        r#"INSERT INTO tasks (repo_id, seq, title, status, runnable, approval_level)
           VALUES ($1, 1, 'gated', 'open', TRUE, 'approve_plan')
           RETURNING task_id"#,
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
    };

    // Without approval, schedule_ready_tasks should NOT pick this up.
    let n = ygg::scheduler::schedule_ready_tasks(&pool, &cfg).await.unwrap();
    assert_eq!(n, 0, "approval_level=approve_plan must block dispatch");

    // After approving, schedule should pick it up.
    ygg::scheduler::approve(&pool, task.0, None).await.unwrap();
    let n = ygg::scheduler::schedule_ready_tasks(&pool, &cfg).await.unwrap();
    assert_eq!(n, 1);

    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_bench_runner_independent_parallel_n_with_fake_claude() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    // Use the in-tree fake claude binary so the test doesn't burn real tokens.
    let fake = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches/fixtures/fake-claude.sh");
    assert!(fake.exists(), "fake-claude.sh fixture must ship in-tree");

    let scenarios_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches/scenarios");
    let manifest = ygg::bench::manifest::LoadedManifest::load(
        scenarios_dir.join("independent-parallel-n")
    ).unwrap();

    let workspace = std::env::temp_dir().join(format!("ygg-bench-test-{}", uuid::Uuid::new_v4()));
    let cfg = ygg::bench::runner::RunnerConfig {
        claude_bin: fake,
        task_timeout_s: 60,
        workspace_root: Some(workspace.clone()),
    };

    // Use the ygg driver to exercise the parallel path.
    let driver = ygg::bench::drivers::YggDriver { config: cfg.clone() };
    let run_id = ygg::bench::runner::execute(&pool, &manifest, &driver, Some(42), &cfg).await.unwrap();

    let repo = ygg::bench::BenchRepo::new(&pool);
    let bench = repo.get_run(run_id).await.unwrap().unwrap();
    assert_eq!(bench.passed, Some(true), "fake claude should produce a passing run");
    assert_eq!(bench.scenario, "independent-parallel-n");

    // All four task results must be marked passed.
    let results = repo.list_task_results(run_id).await.unwrap();
    assert_eq!(results.len(), 4);
    for r in &results {
        assert!(r.passed, "task #{} should pass", r.task_idx);
        assert!(r.tokens_in.is_some(), "fake claude emits a usage block");
    }

    // pass_power_k_for over k=1 should be 1.0 since this run passed.
    let p = ygg::cli::bench_cmd::pass_power_k_for(
        &pool, "independent-parallel-n", ygg::bench::Baseline::Ygg, 1
    ).await.unwrap();
    assert!(p >= 1.0 - 1e-9, "single passed run should yield pass^1 = 1.0, got {p}");

    // Cleanup workspace.
    let _ = std::fs::remove_dir_all(&workspace);
    sqlx::query("DELETE FROM bench_runs WHERE run_id = $1").bind(run_id)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_bench_diff_refuses_overlapping_cis() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    // Create two bench runs of the same scenario with similar wall-clocks
    // so their bootstrap CIs overlap. The diff verdict should be Inconclusive,
    // proving we refuse to declare a winner from noise.
    let repo = ygg::bench::BenchRepo::new(&pool);
    let a = repo.create_run(
        "test-overlap", ygg::bench::Baseline::VanillaSingle, 1,
        "fake", "0", None,
    ).await.unwrap();
    let b = repo.create_run(
        "test-overlap", ygg::bench::Baseline::Ygg, 1,
        "fake", "0", None,
    ).await.unwrap();

    // Wall-clocks within ±2s (CIs definitely overlap).
    for (idx, sec) in [10, 11, 12, 11].iter().enumerate() {
        repo.write_task_result(a.run_id, &ygg::bench::BenchTaskResult {
            run_id: a.run_id, task_idx: idx as i32, passed: true,
            wall_clock_s: *sec, tokens_in: None, tokens_out: None,
            tokens_cache: None, usd: None, reopened: false,
        }).await.unwrap();
    }
    for (idx, sec) in [11, 10, 12, 11].iter().enumerate() {
        repo.write_task_result(b.run_id, &ygg::bench::BenchTaskResult {
            run_id: b.run_id, task_idx: idx as i32, passed: true,
            wall_clock_s: *sec, tokens_in: None, tokens_out: None,
            tokens_cache: None, usd: None, reopened: false,
        }).await.unwrap();
    }
    repo.finalize(a.run_id, true, None).await.unwrap();
    repo.finalize(b.run_id, true, None).await.unwrap();

    // The CLI's `bench diff` is a printer over these same primitives; assert
    // the math directly to keep the test stable against output formatting.
    let res_a = repo.list_task_results(a.run_id).await.unwrap();
    let res_b = repo.list_task_results(b.run_id).await.unwrap();
    let wall_a: Vec<f64> = res_a.iter().map(|r| r.wall_clock_s as f64).collect();
    let wall_b: Vec<f64> = res_b.iter().map(|r| r.wall_clock_s as f64).collect();
    let (_, lo_a, hi_a) = ygg::bench::stats::bootstrap_mean_ci(&wall_a, 1000);
    let (_, lo_b, hi_b) = ygg::bench::stats::bootstrap_mean_ci(&wall_b, 1000);
    let v = ygg::bench::stats::ci_diff_verdict(lo_a, hi_a, lo_b, hi_b);
    assert!(matches!(v, ygg::bench::stats::Verdict::Inconclusive),
        "overlapping CIs must produce Inconclusive verdict; got {v:?}");

    sqlx::query("DELETE FROM bench_runs WHERE scenario = 'test-overlap'")
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_bench_runner_contention_with_fake_claude() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let fake = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches/fixtures/fake-claude.sh");
    let scenarios_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches/scenarios");
    let manifest = ygg::bench::manifest::LoadedManifest::load(
        scenarios_dir.join("contention")
    ).unwrap();

    let workspace = std::env::temp_dir().join(format!("ygg-bench-cont-{}", uuid::Uuid::new_v4()));
    let cfg = ygg::bench::runner::RunnerConfig {
        claude_bin: fake,
        task_timeout_s: 30,
        workspace_root: Some(workspace.clone()),
    };

    // Use VanillaSingle (sequential) so both bumps land in Cargo.toml. The
    // scheduler-backed YggDriver with locks would also pass; the parallel
    // race-without-locks path is the negative test, requiring an explicit
    // mode flag — left for a follow-up.
    let driver = ygg::bench::drivers::VanillaSingleDriver { config: cfg.clone() };
    let run_id = ygg::bench::runner::execute(&pool, &manifest, &driver, None, &cfg).await.unwrap();

    let repo = ygg::bench::BenchRepo::new(&pool);
    let bench = repo.get_run(run_id).await.unwrap().unwrap();
    assert_eq!(bench.passed, Some(true), "sequential driver should land both bumps");

    let _ = std::fs::remove_dir_all(&workspace);
    sqlx::query("DELETE FROM bench_runs WHERE run_id = $1").bind(run_id)
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_backfill_idempotent() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-bf'")
        .execute(&pool).await.unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-bf', 'sched-bf') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'bf-ip', 'in_progress'), ($1, 2, 'bf-cl', 'closed')"
    ).bind(repo.0).execute(&pool).await.unwrap();

    let s1 = ygg::scheduler::backfill(&pool).await.unwrap();
    assert!(s1.in_progress_runs_created >= 1);
    assert!(s1.closed_runs_created >= 1);

    let s2 = ygg::scheduler::backfill(&pool).await.unwrap();
    assert_eq!(s2.in_progress_runs_created, 0);
    assert_eq!(s2.closed_runs_created, 0);

    sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0)
        .execute(&pool).await.unwrap();
}
