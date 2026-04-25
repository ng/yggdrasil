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
    TaskCreated,
    TaskStatusChanged,
    Remembered,
    EmbeddingCacheHit,
    ClassifierDecision,
    ScoringDecision,
    RedactionApplied,
    HitReferenced,
    AgentStateChanged,
    Message,
    RunScheduled,
    RunClaimed,
    RunTerminal,
    RunRetry,
    SchedulerTick,
    SchedulerError,
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
            Self::TaskCreated      => "task_created",
            Self::TaskStatusChanged => "task_status",
            Self::Remembered       => "remembered",
            Self::EmbeddingCacheHit => "cache_hit",
            Self::ClassifierDecision => "classifier",
            Self::ScoringDecision => "scoring",
            Self::RedactionApplied => "redacted",
            Self::HitReferenced => "referenced",
            Self::AgentStateChanged => "agent_state",
            Self::Message => "message",
            Self::RunScheduled => "run_scheduled",
            Self::RunClaimed => "run_claimed",
            Self::RunTerminal => "run_terminal",
            Self::RunRetry => "run_retry",
            Self::SchedulerTick => "scheduler_tick",
            Self::SchedulerError => "scheduler_error",
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

/// Claude Code session id, read lazily from the environment. The hook scripts
/// export CLAUDE_SESSION_ID; spawn/inject/digest inherit it from the shell
/// that invoked them. Missing env var => None, the column stays NULL.
pub fn cc_session_id() -> Option<String> {
    std::env::var("CLAUDE_SESSION_ID").ok().filter(|s| !s.is_empty())
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
        // Auto-tag with the ambient CC session id when the hook path set it.
        // Keeps every emit() callsite untouched.
        let cc_session_id = cc_session_id();
        sqlx::query(
            "INSERT INTO events (event_kind, agent_id, agent_name, payload, cc_session_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&kind)
        .bind(agent_id)
        .bind(agent_name)
        .bind(payload)
        .bind(cc_session_id)
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
