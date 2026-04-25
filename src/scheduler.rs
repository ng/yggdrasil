//! `ygg scheduler` — single authoritative daemon that advances the task DAG.
//! ADR 0016 + docs/design/scheduler.md.
//!
//! Stage 1 MVP (yggdrasil-98): dispatch_ready + finalize_terminal_runs.
//! Heartbeat reap, deadline enforcement, retry, loop detection, poison, and
//! per-repo/epic budgets land per yggdrasil-99 / 100.
//!
//! The scheduler is the only writer of `task_runs.state` past `running`. The
//! Stop hook (yggdrasil-97) writes outcome fields and may transition the run
//! to terminal in manual mode; the scheduler treats already-terminal runs as
//! already-finalized and is therefore safe to run alongside manual workflows.

use crate::config::AppConfig;
use crate::models::event::{EventKind, EventRepo};
use crate::models::task_run::{idempotency_key_for, RunReason, RunState};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// Scheduler advisory-lock id. `pg_try_advisory_lock(SCHEDULER_LOCK_ID)` at
/// startup gives us the singleton invariant; concurrent attempts fail fast
/// with a visible error.
const SCHEDULER_LOCK_ID: i64 = 0x4347_4753_4348; // "GGSCH"

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub tick_interval: Duration,
    /// Global cap on concurrent running attempts.
    pub max_concurrent: i64,
    pub default_max_attempts: i32,
    pub default_heartbeat_ttl_s: i32,
}

impl SchedulerConfig {
    pub fn from_app(_app: &AppConfig) -> Self {
        // Read overrides from env. Defaults are conservative; users opt into
        // higher concurrency once they've watched a tick or two.
        let tick_ms: u64 = std::env::var("YGG_SCHEDULER_TICK_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(2_000);
        let max_concurrent: i64 = std::env::var("YGG_SCHEDULER_MAX_CONCURRENT")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(4);
        Self {
            tick_interval: Duration::from_millis(tick_ms),
            max_concurrent,
            default_max_attempts: 3,
            default_heartbeat_ttl_s: 300,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TickStats {
    pub finalized: i64,
    pub scheduled: i64,
    pub dispatched: i64,
}

/// Run one scheduler tick. Public so `ygg scheduler tick` can call it
/// synchronously for testing without spinning up the daemon loop.
pub async fn tick(pool: &PgPool, cfg: &SchedulerConfig) -> Result<TickStats, anyhow::Error> {
    let mut stats = TickStats::default();
    stats.finalized = finalize_terminal_runs(pool).await?;
    stats.scheduled = schedule_ready_tasks(pool, cfg).await?;
    stats.dispatched = dispatch_ready(pool, cfg).await?;
    Ok(stats)
}

/// Run the scheduler loop until shutdown. Holds the singleton advisory lock
/// for the lifetime of the connection.
pub async fn run(pool: PgPool, cfg: SchedulerConfig) -> Result<(), anyhow::Error> {
    let _guard = acquire_advisory_lock(&pool).await?;
    tracing::info!(
        tick_ms = cfg.tick_interval.as_millis() as u64,
        max_concurrent = cfg.max_concurrent,
        "scheduler started"
    );
    emit_simple_event(&pool, EventKind::SchedulerTick, serde_json::json!({
        "started": true,
        "max_concurrent": cfg.max_concurrent,
    })).await.ok();

    loop {
        // Sleep + ctrl-c handling. LISTEN/NOTIFY wake-up lands per yggdrasil-99
        // (it requires a dedicated listener task; for MVP a simple sleep is
        // fine — the tick interval defaults to 2s).
        let sleep = tokio::time::sleep(cfg.tick_interval);
        tokio::pin!(sleep);
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("scheduler stopping (ctrl-c)");
                break;
            }
            _ = &mut sleep => {}
        }

        match tick(&pool, &cfg).await {
            Ok(stats) => {
                if stats.finalized + stats.scheduled + stats.dispatched > 0 {
                    tracing::info!(
                        finalized = stats.finalized,
                        scheduled = stats.scheduled,
                        dispatched = stats.dispatched,
                        "scheduler tick"
                    );
                    emit_simple_event(&pool, EventKind::SchedulerTick,
                        serde_json::json!(stats)).await.ok();
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "scheduler tick failed");
                emit_simple_event(&pool, EventKind::SchedulerError, serde_json::json!({
                    "error": err.to_string(),
                })).await.ok();
            }
        }
    }

    Ok(())
}

/// (1) Find tasks whose current run reached a terminal state but whose task
/// status is still in_progress, and close the task accordingly. Idempotent.
/// Stage 1 only: succeeded → closed; failed/crashed → closed with reason
/// "failed" (no retry yet — that's yggdrasil-99).
pub async fn finalize_terminal_runs(pool: &PgPool) -> Result<i64, anyhow::Error> {
    // Pull the latest run per task where state is terminal and the task
    // hasn't been closed yet. We skip-locked so concurrent runs of `tick`
    // (shouldn't happen; advisory lock — but be safe) cooperate.
    // No FOR UPDATE: finalize is idempotent (concurrent ticks would do the
    // same UPDATE harmlessly). FOR UPDATE on a CTE isn't allowed in pg anyway.
    let rows: Vec<(Uuid, RunState, RunReason, i32, Option<String>)> = sqlx::query_as(
        r#"SELECT DISTINCT ON (tr.task_id)
                  tr.task_id, tr.state, tr.reason, tr.attempt, tr.output_commit_sha
             FROM task_runs tr
             JOIN tasks t ON t.task_id = tr.task_id
            WHERE tr.state IN ('succeeded', 'failed', 'crashed', 'cancelled', 'poison')
              AND t.status IN ('open', 'in_progress')
            ORDER BY tr.task_id, tr.attempt DESC
            LIMIT 100"#,
    )
    .fetch_all(pool)
    .await?;

    let mut closed = 0i64;
    for (task_id, state, reason, attempt, commit) in rows {
        let close_reason = match state {
            RunState::Succeeded => format!("run #{attempt} succeeded"),
            RunState::Failed     => format!("run #{attempt} failed: {reason}"),
            RunState::Crashed    => format!("run #{attempt} crashed: {reason}"),
            RunState::Cancelled  => format!("run #{attempt} cancelled"),
            RunState::Poison     => format!("run #{attempt} poisoned: {reason}"),
            _ => continue,
        };

        let mut tx = pool.begin().await?;
        sqlx::query(
            r#"UPDATE tasks
               SET status = 'closed',
                   closed_at = COALESCE(closed_at, now()),
                   close_reason = $2,
                   current_attempt_id = NULL,
                   result_blob_ref = COALESCE(result_blob_ref, $3),
                   updated_at = now()
               WHERE task_id = $1"#,
        )
        .bind(task_id)
        .bind(&close_reason)
        .bind(commit.as_deref())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO task_events (task_id, kind, payload) VALUES ($1, 'status_change', $2)",
        )
        .bind(task_id)
        .bind(serde_json::json!({
            "to": "closed",
            "by": "scheduler",
            "run_state": state.as_str(),
            "run_reason": reason.as_str(),
            "attempt": attempt,
            "commit_sha": commit,
        }))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        closed += 1;
    }
    Ok(closed)
}

/// (2) For tasks marked `runnable=TRUE` with no current_attempt_id and all
/// blockers closed, insert a fresh `task_runs` row at state='ready'. This is
/// where the scheduler "owns" the queue: turning runnable tasks into ready
/// runs that dispatch_ready() will pick up next.
pub async fn schedule_ready_tasks(
    pool: &PgPool,
    cfg: &SchedulerConfig,
) -> Result<i64, anyhow::Error> {
    let rows: Vec<(Uuid, Option<i32>, Option<i64>)> = sqlx::query_as(
        r#"SELECT t.task_id, t.max_attempts, t.timeout_ms
             FROM tasks t
            WHERE t.runnable = TRUE
              AND t.status IN ('open', 'in_progress')
              AND t.current_attempt_id IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM task_deps d
                  JOIN tasks b ON b.task_id = d.blocker_id
                   WHERE d.task_id = t.task_id AND b.status <> 'closed'
              )
              -- Skip when a non-terminal run already exists (e.g. a stuck
              -- 'running' row past heartbeat — that's yggdrasil-99 territory).
              AND NOT EXISTS (
                  SELECT 1 FROM task_runs r
                   WHERE r.task_id = t.task_id
                     AND r.state IN ('scheduled', 'ready', 'running', 'retrying')
              )
              -- Approval gate: tasks at approve_plan/completion need a
              -- positive approved_at before the scheduler will dispatch.
              AND (t.approval_level = 'auto' OR t.approved_at IS NOT NULL)
            ORDER BY t.priority, t.updated_at
            FOR UPDATE OF t SKIP LOCKED
            LIMIT 100"#,
    )
    .fetch_all(pool)
    .await?;

    let mut scheduled = 0i64;
    for (task_id, max_attempts, timeout_ms) in rows {
        let prev_attempt: Option<i32> = sqlx::query_scalar(
            "SELECT MAX(attempt) FROM task_runs WHERE task_id = $1",
        )
        .bind(task_id)
        .fetch_one(pool)
        .await
        .ok()
        .flatten();
        let attempt = prev_attempt.unwrap_or(0) + 1;
        let max = max_attempts.unwrap_or(cfg.default_max_attempts);

        let deadline = timeout_ms.map(|ms|
            chrono::Utc::now() + chrono::Duration::milliseconds(ms.max(0))
        );

        let key = idempotency_key_for(task_id, attempt);
        // Insert directly as 'ready'. If we crash mid-tick the row is on disk
        // and the next tick picks it up via dispatch_ready.
        let row: Option<Uuid> = sqlx::query_scalar(
            r#"INSERT INTO task_runs
               (task_id, attempt, idempotency_key, state, max_attempts,
                heartbeat_ttl_s, deadline_at, input)
               VALUES ($1, $2, $3, 'ready', $4, $5, $6, $7)
               ON CONFLICT (idempotency_key) DO NOTHING
               RETURNING run_id"#,
        )
        .bind(task_id)
        .bind(attempt)
        .bind(&key)
        .bind(max)
        .bind(cfg.default_heartbeat_ttl_s)
        .bind(deadline)
        .bind(serde_json::json!({}))
        .fetch_optional(pool)
        .await?;

        if let Some(run_id) = row {
            scheduled += 1;
            let task_ref = task_ref_for(pool, task_id).await
                .unwrap_or_else(|| task_id.to_string());
            EventRepo::new(pool).emit(
                EventKind::RunScheduled,
                "scheduler",
                None,
                serde_json::json!({
                    "task_ref": task_ref,
                    "run_id": run_id,
                    "attempt": attempt,
                }),
            ).await.ok();
        }
    }
    Ok(scheduled)
}

/// (3) Claim ready runs up to budget; spawn agents; bind run→agent.
pub async fn dispatch_ready(
    pool: &PgPool,
    cfg: &SchedulerConfig,
) -> Result<i64, anyhow::Error> {
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_runs WHERE state = 'running'",
    )
    .fetch_one(pool)
    .await?;
    let budget = (cfg.max_concurrent - live).max(0);
    if budget == 0 {
        return Ok(0);
    }

    // The hot SKIP LOCKED claim. See docs/design/task-runs.md § the SKIP
    // LOCKED claim query.
    let claimed: Vec<(Uuid, Uuid, i32, serde_json::Value)> = sqlx::query_as(
        r#"WITH picked AS (
              SELECT tr.run_id
                FROM task_runs tr
                JOIN tasks t USING (task_id)
               WHERE tr.state = 'ready'
                 AND NOT EXISTS (
                     SELECT 1 FROM task_deps d
                     JOIN tasks bt ON bt.task_id = d.blocker_id
                      WHERE d.task_id = tr.task_id AND bt.status <> 'closed'
                 )
                 AND (t.approval_level = 'auto' OR t.approved_at IS NOT NULL)
               ORDER BY t.priority, tr.scheduled_at
               LIMIT $1
               FOR UPDATE OF tr SKIP LOCKED
           )
           UPDATE task_runs
              SET state = 'running',
                  claimed_at = now(),
                  started_at = now(),
                  heartbeat_at = now(),
                  updated_at = now()
            WHERE run_id IN (SELECT run_id FROM picked)
        RETURNING run_id, task_id, attempt, input"#,
    )
    .bind(budget)
    .fetch_all(pool)
    .await?;

    if claimed.is_empty() {
        return Ok(0);
    }

    let app_cfg = AppConfig::from_env()
        .map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let mut dispatched = 0i64;
    for (run_id, task_id, attempt, _input) in claimed {
        // Pull task title for the spawn prompt + ref for events.
        let row: Option<(String, String, i32, String)> = sqlx::query_as(
            r#"SELECT t.title, r.task_prefix, t.seq, COALESCE(t.description, '')
                 FROM tasks t JOIN repos r USING (repo_id)
                WHERE t.task_id = $1"#,
        )
        .bind(task_id)
        .fetch_optional(pool)
        .await?;
        let Some((title, prefix, seq, desc)) = row else {
            // Task vanished underneath us; mark crashed so we don't retry.
            sqlx::query(
                "UPDATE task_runs SET state = 'crashed', reason = 'dependency_failed',
                                       ended_at = now(), updated_at = now()
                  WHERE run_id = $1",
            )
            .bind(run_id)
            .execute(pool)
            .await?;
            continue;
        };

        let task_ref = format!("{prefix}-{seq}");
        let agent_name = scheduler_agent_name(&prefix, seq, attempt);
        let prompt = format!(
            "[ygg-scheduler] Task {task_ref} (attempt {attempt}): {title}\n\n{desc}\n\n\
             Run `ygg run claim {task_ref}` to bind your session, then implement and \
             commit. The scheduler is watching this run; close the task with \
             `ygg task close {task_ref} --reason '...'` when done."
        );

        // Use crate::cli::spawn::execute. A failure to spawn marks the run
        // crashed so the failure is observable and won't burn a retry budget
        // on a non-recoverable problem (tmux down, no claude binary, ...).
        match crate::cli::spawn::execute(pool, &app_cfg, &prompt, Some(&agent_name)).await {
            Ok(()) => {
                // Bind the run to the agent we just registered (spawn::execute
                // registers the agent before launching tmux).
                let agent_id: Option<Uuid> = sqlx::query_scalar(
                    "SELECT agent_id FROM agents WHERE name = $1 LIMIT 1",
                )
                .bind(&agent_name)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
                let mut tx = pool.begin().await?;
                sqlx::query(
                    "UPDATE task_runs SET agent_id = $2, updated_at = now() WHERE run_id = $1",
                )
                .bind(run_id)
                .bind(agent_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE tasks SET current_attempt_id = $2, updated_at = now() WHERE task_id = $1",
                )
                .bind(task_id)
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;

                EventRepo::new(pool).emit(
                    EventKind::RunClaimed,
                    "scheduler",
                    None,
                    serde_json::json!({
                        "task_ref": task_ref,
                        "run_id": run_id,
                        "attempt": attempt,
                        "agent": agent_name,
                    }),
                ).await.ok();
                dispatched += 1;
            }
            Err(err) => {
                tracing::warn!(error = %err, task_ref = %task_ref, "scheduler spawn failed");
                sqlx::query(
                    "UPDATE task_runs
                        SET state = 'crashed',
                            reason = 'tmux_gone',
                            error = jsonb_build_object('reason_code', 'tmux_gone',
                                                       'message', $2::text),
                            ended_at = now(),
                            updated_at = now()
                      WHERE run_id = $1",
                )
                .bind(run_id)
                .bind(err.to_string())
                .execute(pool)
                .await?;
                EventRepo::new(pool).emit(
                    EventKind::SchedulerError,
                    "scheduler",
                    None,
                    serde_json::json!({
                        "phase": "spawn",
                        "task_ref": task_ref,
                        "run_id": run_id,
                        "error": err.to_string(),
                    }),
                ).await.ok();
            }
        }
    }
    Ok(dispatched)
}

fn scheduler_agent_name(prefix: &str, seq: i32, attempt: i32) -> String {
    let suffix = uuid::Uuid::new_v4().to_string().chars().take(6).collect::<String>();
    format!("ygg-{prefix}-{seq}-a{attempt}-{suffix}")
}

async fn task_ref_for(pool: &PgPool, task_id: Uuid) -> Option<String> {
    sqlx::query_as::<_, (String, i32)>(
        r#"SELECT r.task_prefix, t.seq FROM tasks t
           JOIN repos r USING (repo_id) WHERE t.task_id = $1"#,
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|(p, s)| format!("{p}-{s}"))
}

async fn emit_simple_event(
    pool: &PgPool,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<(), sqlx::Error> {
    EventRepo::new(pool).emit(kind, "scheduler", None, payload).await
}

/// Acquire the singleton advisory lock. Returned guard auto-releases on drop
/// (which closes the connection it holds). A second instance fails fast.
pub async fn acquire_advisory_lock(pool: &PgPool) -> Result<AdvisoryLockGuard, anyhow::Error> {
    let mut conn = pool.acquire().await?.detach();
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SCHEDULER_LOCK_ID)
        .fetch_one(&mut conn)
        .await?;
    if !acquired {
        anyhow::bail!(
            "another ygg scheduler is already running on this database (advisory lock {SCHEDULER_LOCK_ID:#x} held)"
        );
    }
    Ok(AdvisoryLockGuard { _conn: conn })
}

pub struct AdvisoryLockGuard {
    _conn: sqlx::PgConnection,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_name_includes_attempt_and_unique_suffix() {
        let n1 = scheduler_agent_name("ygg", 42, 1);
        let n2 = scheduler_agent_name("ygg", 42, 1);
        assert!(n1.starts_with("ygg-ygg-42-a1-"));
        assert!(n2.starts_with("ygg-ygg-42-a1-"));
        assert_ne!(n1, n2);
    }

    #[test]
    fn config_defaults_reasonable() {
        let app = AppConfig {
            database_url: "test".into(),
            ollama_base_url: "test".into(),
            ollama_embed_model: "test".into(),
            ollama_chat_model: String::new(),
            embedding_dimensions: 384,
            context_limit_tokens: 250_000,
            context_hard_cap_tokens: 300_000,
            lock_ttl_secs: 300,
            heartbeat_interval_secs: 60,
            watcher_interval_secs: 30,
            rtk_binary_path: "rtk".into(),
        };
        unsafe { std::env::remove_var("YGG_SCHEDULER_TICK_MS") };
        unsafe { std::env::remove_var("YGG_SCHEDULER_MAX_CONCURRENT") };
        let cfg = SchedulerConfig::from_app(&app);
        assert_eq!(cfg.tick_interval, Duration::from_millis(2000));
        assert_eq!(cfg.max_concurrent, 4);
        assert_eq!(cfg.default_max_attempts, 3);
    }
}
