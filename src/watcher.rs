use std::time::Duration;
use sqlx::PgPool;

use crate::config::AppConfig;
use crate::lock::LockManager;
use crate::models::agent::{AgentRepo, AgentState};

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

        if reaped > 0 || stale > 0 {
            tracing::info!(reaped_locks = reaped, stale_agents = stale, "watcher tick");
        }

        Ok(())
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
