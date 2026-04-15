use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "agent_state", rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Planning,
    Executing,
    WaitingTool,
    ContextFlush,
    HumanOverride,
    Mediation,
    Error,
    Shutdown,
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Planning => write!(f, "planning"),
            Self::Executing => write!(f, "executing"),
            Self::WaitingTool => write!(f, "waiting_tool"),
            Self::ContextFlush => write!(f, "context_flush"),
            Self::HumanOverride => write!(f, "human_override"),
            Self::Mediation => write!(f, "mediation"),
            Self::Error => write!(f, "error"),
            Self::Shutdown => write!(f, "shutdown"),
        }
    }
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct AgentWorkflow {
    pub agent_id: Uuid,
    pub agent_name: String,
    pub current_state: AgentState,
    pub head_node_id: Option<Uuid>,
    pub digest_id: Option<Uuid>,
    pub context_tokens: i32,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct AgentRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> AgentRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Register a new agent or return existing one by name.
    pub async fn register(&self, name: &str) -> Result<AgentWorkflow, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            INSERT INTO agents (agent_name)
            VALUES ($1)
            ON CONFLICT (agent_name) DO UPDATE SET updated_at = now()
            RETURNING agent_id, agent_name, current_state, head_node_id,
                      digest_id, context_tokens, metadata, created_at, updated_at
            "#,
        )
        .bind(name)
        .fetch_one(self.pool)
        .await
    }

    /// Transition agent state with optimistic concurrency control.
    pub async fn transition(
        &self,
        agent_id: Uuid,
        from: AgentState,
        to: AgentState,
    ) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            UPDATE agents
            SET current_state = $3::agent_state, updated_at = now()
            WHERE agent_id = $1 AND current_state = $2::agent_state
            RETURNING agent_id, agent_name, current_state, head_node_id,
                      digest_id, context_tokens, metadata, created_at, updated_at
            "#,
        )
        .bind(agent_id)
        .bind(&from)
        .bind(&to)
        .fetch_optional(self.pool)
        .await
    }

    /// Update the head node and context token count.
    pub async fn update_head(
        &self,
        agent_id: Uuid,
        head_node_id: Uuid,
        context_tokens: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agents SET head_node_id = $2, context_tokens = $3, updated_at = now() WHERE agent_id = $1",
        )
        .bind(agent_id)
        .bind(head_node_id)
        .bind(context_tokens)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Atomically update head, digest, and token count in a single statement.
    /// Use this instead of separate update_head + set_digest calls.
    pub async fn flush_context(
        &self,
        agent_id: Uuid,
        head_node_id: Uuid,
        digest_id: Uuid,
        context_tokens: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agents SET head_node_id = $2, digest_id = $3, context_tokens = $4, updated_at = now() WHERE agent_id = $1",
        )
        .bind(agent_id)
        .bind(head_node_id)
        .bind(digest_id)
        .bind(context_tokens)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Update the digest reference after a context flush.
    pub async fn set_digest(
        &self,
        agent_id: Uuid,
        digest_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agents SET digest_id = $2, updated_at = now() WHERE agent_id = $1")
            .bind(agent_id)
            .bind(digest_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Get agent by ID.
    pub async fn get(&self, agent_id: Uuid) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at
            FROM agents WHERE agent_id = $1
            "#,
        )
        .bind(agent_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Get agent by name.
    pub async fn get_by_name(&self, name: &str) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at
            FROM agents WHERE agent_name = $1
            "#,
        )
        .bind(name)
        .fetch_optional(self.pool)
        .await
    }

    /// List all agents.
    pub async fn list(&self) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at
            FROM agents ORDER BY created_at
            "#,
        )
        .fetch_all(self.pool)
        .await
    }
}
