use std::process::Command;
use std::time::Duration;
use sqlx::PgPool;

use crate::config::AppConfig;
use crate::lock::LockManager;
use crate::models::agent::{AgentRepo, AgentState};
use crate::models::worker::{Worker, WorkerRepo, WorkerState};

/// Background watcher daemon.
/// Periodically: reap expired locks, flag stale agents, cleanup.
pub struct Watcher {
    pool: PgPool,
    config: AppConfig,
}

impl Watcher {
    pub fn new(pool: PgPool, config: AppConfig) -> Self {
        Self { pool, config }
    }

    /// Main loop — runs until SIGTERM/SIGINT.
    pub async fn run(&self) -> Result<(), anyhow::Error> {
        let interval = Duration::from_secs(self.config.watcher_interval_secs);
        tracing::info!(interval_secs = self.config.watcher_interval_secs, "watcher started");

        let mut tick = tokio::time::interval(interval);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.tick().await {
                        tracing::error!(error = %e, "watcher tick failed");
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("watcher shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn tick(&self) -> Result<(), anyhow::Error> {
        let reaped = self.reap_expired_locks().await?;
        let stale = self.flag_stale_agents().await?;
        let worker_updates = self.observe_workers().await.unwrap_or(0);

        if reaped > 0 || stale > 0 || worker_updates > 0 {
            tracing::info!(
                reaped_locks = reaped, stale_agents = stale,
                worker_updates = worker_updates, "watcher tick"
            );
        }

        Ok(())
    }

    /// Observer loop for click-to-do workers. Lists tmux windows in the
    /// `yggdrasil` session, cross-checks against live worker rows, and:
    ///   - touches last_seen_at for matches
    ///   - captures the pane and scans for prompt markers → needs_attention
    ///   - marks rows whose window is gone → abandoned
    async fn observe_workers(&self) -> Result<u64, anyhow::Error> {
        let workers = WorkerRepo::new(&self.pool).list_live().await
            .unwrap_or_default();
        if workers.is_empty() { return Ok(0); }

        // Group by tmux_session so we make one list-windows call per.
        use std::collections::{HashMap, HashSet};
        let mut by_session: HashMap<String, Vec<Worker>> = HashMap::new();
        for w in workers {
            by_session.entry(w.tmux_session.clone()).or_default().push(w);
        }

        let repo = WorkerRepo::new(&self.pool);
        let mut changes = 0u64;

        for (session, ws) in by_session {
            let windows: HashSet<String> = list_tmux_windows(&session).into_iter().collect();
            for w in ws {
                if !windows.contains(&w.tmux_window) {
                    // Window vanished — machine restart, manual kill, or
                    // claude exited and the shell closed.
                    let _ = repo.set_state(
                        w.worker_id, WorkerState::Abandoned,
                        Some("tmux window absent on observer tick"),
                    ).await;
                    changes += 1;
                    continue;
                }

                // Touch last_seen_at; then inspect the pane for prompts.
                let _ = repo.touch(w.worker_id).await;
                let pane = capture_pane(&session, &w.tmux_window).unwrap_or_default();
                let next = classify_pane(&pane);
                if next != w.state {
                    let _ = repo.set_state(w.worker_id, next, None).await;
                    changes += 1;
                }
            }
        }
        Ok(changes)
    }

    /// Remove all expired locks.
    async fn reap_expired_locks(&self) -> Result<u64, anyhow::Error> {
        let lock_mgr = LockManager::new(&self.pool, self.config.lock_ttl_secs);
        let count = lock_mgr.reap_expired().await?;
        Ok(count)
    }

    /// Flag agents whose updated_at is older than 2x TTL as potentially dead.
    async fn flag_stale_agents(&self) -> Result<u64, anyhow::Error> {
        let stale_threshold = (self.config.lock_ttl_secs * 2) as i64;

        let stale_agents: Vec<_> = sqlx::query_as::<_, crate::models::agent::AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at
            FROM agents
            WHERE current_state IN ('executing', 'waiting_tool', 'planning')
              AND updated_at < now() - make_interval(secs => $1)
            "#,
        )
        .bind(stale_threshold as f64)
        .fetch_all(&self.pool)
        .await?;

        let agent_repo = AgentRepo::new(&self.pool);
        let mut count = 0u64;

        for agent in stale_agents {
            tracing::warn!(
                agent = %agent.agent_name,
                last_update = %agent.updated_at,
                "flagging stale agent"
            );
            agent_repo
                .transition(agent.agent_id, agent.current_state, AgentState::Error)
                .await?;
            count += 1;
        }

        Ok(count)
    }
}

/// `tmux list-windows -t <session> -F '#{window_name}'` → Vec<name>.
/// Empty on any error (session missing, tmux absent) — the observer
/// treats that as "all workers abandoned," which is correct.
fn list_tmux_windows(session: &str) -> Vec<String> {
    let out = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_name}"])
        .output();
    let Ok(out) = out else { return Vec::new(); };
    if !out.status.success() { return Vec::new(); }
    String::from_utf8_lossy(&out.stdout)
        .lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn capture_pane(session: &str, window: &str) -> Option<String> {
    let target = format!("{session}:{window}");
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", &target, "-S", "-200"])
        .output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Classify the last ~200 lines of the pane into a WorkerState. Looks
/// for Claude Code / Codex prompt markers first, then idle heuristics.
fn classify_pane(pane: &str) -> WorkerState {
    // Trust-dialog / permission variants (shouldn't happen post pre-trust,
    // but cheap to detect if Claude changes its schema out from under us).
    const ATTENTION: &[&str] = &[
        "Do you want to",
        "Bypass permissions",
        "trust this folder",
        "Quick safety check",
        "Do you trust",
        "Continue? [y/n]",
        "Select an option",
    ];
    for m in ATTENTION {
        if pane.contains(m) { return WorkerState::NeedsAttention; }
    }

    // "Active" heuristic: the Claude Code UI shows a thinking indicator
    // or streaming content; very recent newlines indicate activity. We
    // proxy with "is there a recognisable Claude prompt line in the
    // last 40 lines?" — if so, Claude is active.
    let tail: String = pane.lines().rev().take(40).collect::<Vec<_>>().join("\n");
    if tail.contains("│ >") || tail.contains("Ctrl-C") || tail.contains("esc to interrupt") {
        return WorkerState::Running;
    }

    // Fell through: claude may have exited or is just quiet.
    WorkerState::Idle
}
