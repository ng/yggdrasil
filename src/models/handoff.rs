//! Session handoffs — an agent's resume note across a `/clear`. One row per
//! (repo_id, agent_id); `save` replaces any prior note so there is always
//! exactly one current handoff. Plain text, no embeddings: surfaced at the top
//! of `ygg prime` (SessionStart) and via `ygg handoff show`. Agent identity is
//! durable (ADR 0013), so the note survives the context reset and re-attaches
//! to the same agent on the next session.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Handoff {
    pub handoff_id: Uuid,
    pub repo_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

pub struct HandoffRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> HandoffRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Replace any prior handoff for (repo_id, agent_id) and store the new one.
    /// `IS NOT DISTINCT FROM` so NULL repo/agent match each other rather than
    /// accumulating duplicate rows.
    pub async fn save(
        &self,
        repo_id: Option<Uuid>,
        agent_id: Option<Uuid>,
        text: &str,
    ) -> Result<Handoff, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM handoffs
             WHERE repo_id IS NOT DISTINCT FROM $1 AND agent_id IS NOT DISTINCT FROM $2",
        )
        .bind(repo_id)
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
        let handoff = sqlx::query_as::<_, Handoff>(
            r#"INSERT INTO handoffs (repo_id, agent_id, text)
               VALUES ($1, $2, $3)
               RETURNING handoff_id, repo_id, agent_id, text, created_at"#,
        )
        .bind(repo_id)
        .bind(agent_id)
        .bind(text)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(handoff)
    }

    /// The current handoff for (repo_id, agent_id), if any.
    pub async fn latest(
        &self,
        repo_id: Option<Uuid>,
        agent_id: Option<Uuid>,
    ) -> Result<Option<Handoff>, sqlx::Error> {
        sqlx::query_as::<_, Handoff>(
            r#"SELECT handoff_id, repo_id, agent_id, text, created_at
               FROM handoffs
               WHERE repo_id IS NOT DISTINCT FROM $1 AND agent_id IS NOT DISTINCT FROM $2
               ORDER BY created_at DESC
               LIMIT 1"#,
        )
        .bind(repo_id)
        .bind(agent_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Delete the handoff(s) for (repo_id, agent_id). Returns true if any went.
    pub async fn clear(
        &self,
        repo_id: Option<Uuid>,
        agent_id: Option<Uuid>,
    ) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "DELETE FROM handoffs
             WHERE repo_id IS NOT DISTINCT FROM $1 AND agent_id IS NOT DISTINCT FROM $2",
        )
        .bind(repo_id)
        .bind(agent_id)
        .execute(self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }
}
