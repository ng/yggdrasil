use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Session {
    pub session_id: Uuid,
    pub agent_id: Uuid,
    pub repo_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub metadata: serde_json::Value,
}

pub struct SessionRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> SessionRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn start(
        &self,
        agent_id: Uuid,
        repo_id: Option<Uuid>,
    ) -> Result<Session, sqlx::Error> {
        sqlx::query_as::<_, Session>(
            r#"INSERT INTO sessions (agent_id, repo_id)
               VALUES ($1, $2)
               RETURNING session_id, agent_id, repo_id, started_at, ended_at, metadata"#,
        )
        .bind(agent_id)
        .bind(repo_id)
        .fetch_one(self.pool)
        .await
    }

    pub async fn end(&self, session_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE sessions SET ended_at = now() WHERE session_id = $1 AND ended_at IS NULL")
            .bind(session_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn latest_for_agent(
        &self,
        agent_id: Uuid,
    ) -> Result<Option<Session>, sqlx::Error> {
        sqlx::query_as::<_, Session>(
            r#"SELECT session_id, agent_id, repo_id, started_at, ended_at, metadata
               FROM sessions
               WHERE agent_id = $1
               ORDER BY started_at DESC
               LIMIT 1"#,
        )
        .bind(agent_id)
        .fetch_optional(self.pool)
        .await
    }
}
