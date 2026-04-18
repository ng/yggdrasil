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
    #[sqlx(default)]
    pub persona: Option<String>,
}

impl AgentWorkflow {
    /// Display key — `name` when persona is unset, `name:persona` otherwise.
    /// Use when rendering to humans; prefer `(agent_name, persona)` for
    /// lookups and joins.
    pub fn display_name(&self) -> String {
        match &self.persona {
            Some(p) if !p.is_empty() => format!("{}:{p}", self.agent_name),
            _ => self.agent_name.clone(),
        }
    }
}

pub struct AgentRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> AgentRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Register a new agent or return existing one by (name, persona).
    /// The compound key means the same cwd can host multiple personas.
    pub async fn register(&self, name: &str) -> Result<AgentWorkflow, sqlx::Error> {
        self.register_with_persona(name, None).await
    }

    pub async fn register_with_persona(
        &self,
        name: &str,
        persona: Option<&str>,
    ) -> Result<AgentWorkflow, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            INSERT INTO agents (agent_name, persona)
            VALUES ($1, $2)
            ON CONFLICT (agent_name, COALESCE(persona, ''))
              DO UPDATE SET updated_at = now()
            RETURNING agent_id, agent_name, current_state, head_node_id,
                      digest_id, context_tokens, metadata, created_at, updated_at, persona
            "#,
        )
        .bind(name)
        .bind(persona)
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
                      digest_id, context_tokens, metadata, created_at, updated_at, persona
            "#,
        )
        .bind(agent_id)
        .bind(&from)
        .bind(&to)
        .fetch_optional(self.pool)
        .await
    }

    /// Set the agent's state unconditionally, optionally recording the tool
    /// it's waiting on. Hook-driven state updates can't guess the current
    /// state, so they use this rather than the OCC transition(). Emits an
    /// `agent_state_changed` event when the state actually changes so the
    /// dashboard timeline can render transitions.
    pub async fn force_state(
        &self,
        agent_id: Uuid,
        to: AgentState,
        last_tool: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let meta_patch = match last_tool {
            Some(t) => serde_json::json!({"last_tool": t}),
            None => serde_json::json!({"last_tool": null}),
        };
        // Returns (old_state, agent_name) so we can emit a transition event.
        let row: Option<(AgentState, String)> = sqlx::query_as(
            r#"
            WITH prior AS (
                SELECT current_state AS old_state, agent_name FROM agents WHERE agent_id = $1
            )
            UPDATE agents
               SET current_state = $2::agent_state,
                   metadata = metadata || $3::jsonb,
                   updated_at = now()
             WHERE agent_id = $1
             RETURNING (SELECT old_state FROM prior) AS old_state,
                       (SELECT agent_name FROM prior) AS agent_name
            "#,
        )
        .bind(agent_id)
        .bind(&to)
        .bind(meta_patch)
        .fetch_optional(self.pool)
        .await?;

        // Emit only on actual change — no-op transitions aren't useful signal.
        if let Some((old, name)) = row {
            if old != to {
                let payload = serde_json::json!({
                    "from": old.to_string(),
                    "to": to.to_string(),
                    "tool": last_tool,
                });
                let _ = sqlx::query(
                    "INSERT INTO events (event_kind, agent_id, agent_name, payload, cc_session_id)
                     VALUES ('agent_state_changed', $1, $2, $3, $4)"
                )
                .bind(agent_id)
                .bind(&name)
                .bind(payload)
                .bind(crate::models::event::cc_session_id())
                .execute(self.pool)
                .await;
            }
        }
        Ok(())
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
                   digest_id, context_tokens, metadata, created_at, updated_at, persona
            FROM agents WHERE agent_id = $1
            "#,
        )
        .bind(agent_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Get agent by name.
    /// Lookup by bare agent_name. When multiple personas share the name,
    /// returns the one with NULL persona if it exists, else the most
    /// recently updated. Callers that care about persona should use
    /// `get_by_name_persona`.
    pub async fn get_by_name(&self, name: &str) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at, persona
            FROM agents WHERE agent_name = $1
            ORDER BY (persona IS NOT NULL), updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(name)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn get_by_name_persona(
        &self,
        name: &str,
        persona: Option<&str>,
    ) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at, persona
            FROM agents
            WHERE agent_name = $1 AND COALESCE(persona, '') = COALESCE($2, '')
            "#,
        )
        .bind(name)
        .bind(persona)
        .fetch_optional(self.pool)
        .await
    }

    /// List all agents.
    /// List live agents only — the dashboard/statusline default.
    pub async fn list(&self) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at, persona
            FROM agents
            WHERE archived_at IS NULL
            ORDER BY created_at
            "#,
        )
        .fetch_all(self.pool)
        .await
    }

    /// Include archived agents — for `ygg agent list --all` and audit paths.
    pub async fn list_all(&self) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at, persona
            FROM agents ORDER BY created_at
            "#,
        )
        .fetch_all(self.pool)
        .await
    }

    pub async fn archive(&self, agent_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agents SET archived_at = now() WHERE agent_id = $1")
            .bind(agent_id).execute(self.pool).await?;
        Ok(())
    }

    pub async fn unarchive(&self, agent_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agents SET archived_at = NULL WHERE agent_id = $1")
            .bind(agent_id).execute(self.pool).await?;
        Ok(())
    }

    /// Return the number of stale live agents: no events, no sessions
    /// started, in the last `days`. The result is how many SHOULD be
    /// archived; caller decides to actually do it.
    pub async fn find_stale(&self, days: i64) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT a.agent_id, a.agent_name, a.current_state, a.head_node_id,
                   a.digest_id, a.context_tokens, a.metadata, a.created_at, a.updated_at
            FROM agents a
            WHERE a.archived_at IS NULL
              AND a.updated_at < now() - ($1 || ' days')::interval
              AND NOT EXISTS (
                    SELECT 1 FROM events e
                     WHERE e.agent_id = a.agent_id
                       AND e.created_at >= now() - ($1 || ' days')::interval
              )
              AND NOT EXISTS (
                    SELECT 1 FROM sessions s
                     WHERE s.agent_id = a.agent_id
                       AND COALESCE(s.ended_at, s.started_at)
                            >= now() - ($1 || ' days')::interval
              )
              AND NOT EXISTS (
                    SELECT 1 FROM locks l
                     WHERE l.agent_id = a.agent_id AND l.expires_at > now()
              )
            ORDER BY a.updated_at
            "#,
        )
        .bind(days.to_string())
        .fetch_all(self.pool)
        .await
    }

    /// Find orphaned agents stuck in active states (crash recovery).
    /// Returns agents in Executing/WaitingTool/Planning that haven't been
    /// updated within the staleness threshold.
    pub async fn find_orphaned(&self, stale_secs: i64) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state, head_node_id,
                   digest_id, context_tokens, metadata, created_at, updated_at, persona
            FROM agents
            WHERE current_state IN ('executing', 'waiting_tool', 'planning', 'context_flush')
              AND updated_at < now() - make_interval(secs => $1)
            ORDER BY updated_at
            "#,
        )
        .bind(stale_secs as f64)
        .fetch_all(self.pool)
        .await
    }

    /// Reset an orphaned agent to Idle for resume.
    pub async fn reset_to_idle(&self, agent_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agents SET current_state = 'idle', updated_at = now() WHERE agent_id = $1",
        )
        .bind(agent_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}
