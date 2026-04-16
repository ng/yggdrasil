use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "event_kind", rename_all = "snake_case")]
pub enum EventKind {
    NodeWritten,
    LockAcquired,
    LockReleased,
    DigestWritten,
    SimilarityHit,
    CorrectionDetected,
    HookFired,
    EmbeddingCall,
}

impl EventKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NodeWritten       => "node_written",
            Self::LockAcquired     => "lock_acquired",
            Self::LockReleased     => "lock_released",
            Self::DigestWritten    => "digest_written",
            Self::SimilarityHit    => "similarity_hit",
            Self::CorrectionDetected => "correction",
            Self::HookFired        => "hook_fired",
            Self::EmbeddingCall    => "embedding_call",
        }
    }
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Event {
    pub id: Uuid,
    pub event_kind: EventKind,
    pub agent_id: Option<Uuid>,
    pub agent_name: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

pub struct EventRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> EventRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn emit(
        &self,
        kind: EventKind,
        agent_name: &str,
        agent_id: Option<Uuid>,
        payload: serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO events (event_kind, agent_id, agent_name, payload) VALUES ($1, $2, $3, $4)",
        )
        .bind(&kind)
        .bind(agent_id)
        .bind(agent_name)
        .bind(payload)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Fetch events newer than `since`, optionally filtered by agent name.
    pub async fn list_since(
        &self,
        since: DateTime<Utc>,
        agent_name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Event>, sqlx::Error> {
        if let Some(name) = agent_name {
            sqlx::query_as::<_, Event>(
                r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
                   FROM events WHERE created_at > $1 AND agent_name = $2
                   ORDER BY created_at ASC LIMIT $3"#,
            )
            .bind(since).bind(name).bind(limit)
            .fetch_all(self.pool).await
        } else {
            sqlx::query_as::<_, Event>(
                r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
                   FROM events WHERE created_at > $1
                   ORDER BY created_at ASC LIMIT $2"#,
            )
            .bind(since).bind(limit)
            .fetch_all(self.pool).await
        }
    }

    /// Fetch the most recent N events (newest first, for initial display).
    pub async fn list_recent(
        &self,
        limit: i64,
        agent_name: Option<&str>,
    ) -> Result<Vec<Event>, sqlx::Error> {
        if let Some(name) = agent_name {
            sqlx::query_as::<_, Event>(
                r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
                   FROM events WHERE agent_name = $1
                   ORDER BY created_at DESC LIMIT $2"#,
            )
            .bind(name).bind(limit)
            .fetch_all(self.pool).await
        } else {
            sqlx::query_as::<_, Event>(
                r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
                   FROM events ORDER BY created_at DESC LIMIT $1"#,
            )
            .bind(limit)
            .fetch_all(self.pool).await
        }
    }
}
