//! Native Claude Code subagents (ADR 0020), recorded by the `SubagentStart` /
//! `SubagentStop` hooks as children of the parent agent.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// A subagent counts as live while it is `running` and has started or made a
/// tool call within this window. Bounds the effect of a lost `SubagentStop`.
pub const LIVE_WINDOW_SECS: i64 = 30 * 60;

/// Pending Agent tool calls older than this were never started (denied,
/// failed, or the hook was installed mid-call) and are dropped.
const PENDING_TTL_SECS: i64 = 60 * 60;

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Subagent {
    pub subagent_id: Uuid,
    pub parent_agent_id: Uuid,
    pub cc_session_id: String,
    pub cc_agent_id: Option<String>,
    pub agent_type: String,
    pub description: Option<String>,
    pub background: bool,
    pub isolation: Option<String>,
    pub state: String,
    pub last_tool: Option<String>,
    pub transcript_path: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub tool_calls: i32,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl Subagent {
    /// `Explore: probe the readme`, or just the type when no description.
    pub fn label(&self) -> String {
        let kind = if self.agent_type.is_empty() {
            "subagent"
        } else {
            self.agent_type.as_str()
        };
        match self.description.as_deref() {
            Some(d) if !d.is_empty() => format!("{kind}: {d}"),
            _ => kind.to_string(),
        }
    }
}

/// The parent's Agent tool call, as seen by PreToolUse.
pub struct AgentCall<'a> {
    pub tool_use_id: &'a str,
    pub agent_type: &'a str,
    pub description: Option<&'a str>,
    pub background: bool,
    pub isolation: Option<&'a str>,
}

/// What SubagentStop learned from the payload and the subagent's transcript.
#[derive(Default)]
pub struct StopReport<'a> {
    pub transcript_path: Option<&'a str>,
    pub last_message: Option<&'a str>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub tool_calls: i32,
}

const COLUMNS: &str = "subagent_id, parent_agent_id, cc_session_id, cc_agent_id, agent_type, \
     description, background, isolation, state, last_tool, transcript_path, input_tokens, \
     output_tokens, tool_calls, started_at, updated_at, ended_at";

pub struct SubagentRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> SubagentRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// PreToolUse on the Agent tool: remember the call so SubagentStart can
    /// attach its description. Idempotent on (session, tool_use_id).
    pub async fn record_call(
        &self,
        parent_agent_id: Uuid,
        user_id: &str,
        session_id: &str,
        call: &AgentCall<'_>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO subagents (parent_agent_id, user_id, cc_session_id, tool_use_id,
                                   agent_type, description, background, isolation)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (cc_session_id, tool_use_id) WHERE tool_use_id IS NOT NULL
              DO NOTHING
            "#,
        )
        .bind(parent_agent_id)
        .bind(user_id)
        .bind(session_id)
        .bind(call.tool_use_id)
        .bind(call.agent_type)
        .bind(call.description)
        .bind(call.background)
        .bind(call.isolation)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// SubagentStart. Re-marks a known subagent running (resume, or a
    /// repeated hook), otherwise claims the oldest pending Agent call of the
    /// same type in this session, otherwise inserts a bare row.
    pub async fn start(
        &self,
        parent_agent_id: Uuid,
        user_id: &str,
        session_id: &str,
        cc_agent_id: &str,
        agent_type: &str,
        cwd: Option<&str>,
    ) -> Result<Subagent, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "DELETE FROM subagents
              WHERE cc_session_id = $1 AND state = 'pending'
                AND started_at < now() - make_interval(secs => $2)",
        )
        .bind(session_id)
        .bind(PENDING_TTL_SECS as f64)
        .execute(&mut *tx)
        .await?;

        let existing = sqlx::query_as::<_, Subagent>(&format!(
            "UPDATE subagents
                SET state = 'running', ended_at = NULL, updated_at = now(),
                    cwd = COALESCE($3, cwd)
              WHERE cc_session_id = $1 AND cc_agent_id = $2
          RETURNING {COLUMNS}"
        ))
        .bind(session_id)
        .bind(cc_agent_id)
        .bind(cwd)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            tx.commit().await?;
            return Ok(row);
        }

        let claimed = sqlx::query_as::<_, Subagent>(&format!(
            "UPDATE subagents
                SET cc_agent_id = $2, state = 'running', updated_at = now(), cwd = $4
              WHERE subagent_id = (
                    SELECT subagent_id FROM subagents
                     WHERE cc_session_id = $1 AND state = 'pending'
                       AND cc_agent_id IS NULL AND agent_type = $3
                     ORDER BY started_at, subagent_id
                     LIMIT 1
                     FOR UPDATE SKIP LOCKED)
          RETURNING {COLUMNS}"
        ))
        .bind(session_id)
        .bind(cc_agent_id)
        .bind(agent_type)
        .bind(cwd)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = claimed {
            tx.commit().await?;
            return Ok(row);
        }

        let inserted = sqlx::query_as::<_, Subagent>(&format!(
            "INSERT INTO subagents (parent_agent_id, user_id, cc_session_id, cc_agent_id,
                                    agent_type, state, cwd)
             VALUES ($1, $2, $3, $4, $5, 'running', $6)
             ON CONFLICT (cc_session_id, cc_agent_id) WHERE cc_agent_id IS NOT NULL
               DO UPDATE SET state = 'running', ended_at = NULL, updated_at = now()
             RETURNING {COLUMNS}"
        ))
        .bind(parent_agent_id)
        .bind(user_id)
        .bind(session_id)
        .bind(cc_agent_id)
        .bind(agent_type)
        .bind(cwd)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(inserted)
    }

    /// PreToolUse fired inside a subagent: record its tool and keep it live.
    pub async fn touch(
        &self,
        session_id: &str,
        cc_agent_id: &str,
        tool: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE subagents SET last_tool = $3, updated_at = now()
              WHERE cc_session_id = $1 AND cc_agent_id = $2 AND state = 'running'",
        )
        .bind(session_id)
        .bind(cc_agent_id)
        .bind(tool)
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// SubagentStop. Marks the subagent done with its token totals. When no
    /// start was recorded (hook installed mid-run), a done row is inserted —
    /// unless `agent_type` is empty: Claude Code also fires SubagentStop for
    /// internal agents (prompt suggestions, `/btw`) that never had a start,
    /// and those are not the user's subagents. Returns `None` when ignored.
    pub async fn stop(
        &self,
        parent_agent_id: Uuid,
        user_id: &str,
        session_id: &str,
        cc_agent_id: &str,
        agent_type: &str,
        report: &StopReport<'_>,
    ) -> Result<Option<Subagent>, sqlx::Error> {
        let updated = sqlx::query_as::<_, Subagent>(&format!(
            "UPDATE subagents
                SET state = 'done', ended_at = now(), updated_at = now(),
                    transcript_path = COALESCE($3, transcript_path),
                    last_message = COALESCE($4, last_message),
                    input_tokens = $5, output_tokens = $6, cache_read = $7,
                    cache_write = $8, tool_calls = $9
              WHERE cc_session_id = $1 AND cc_agent_id = $2
          RETURNING {COLUMNS}"
        ))
        .bind(session_id)
        .bind(cc_agent_id)
        .bind(report.transcript_path)
        .bind(report.last_message)
        .bind(report.input_tokens)
        .bind(report.output_tokens)
        .bind(report.cache_read)
        .bind(report.cache_write)
        .bind(report.tool_calls)
        .fetch_optional(self.pool)
        .await?;
        if updated.is_some() || agent_type.is_empty() {
            return Ok(updated);
        }

        let inserted = sqlx::query_as::<_, Subagent>(&format!(
            "INSERT INTO subagents (parent_agent_id, user_id, cc_session_id, cc_agent_id,
                                    agent_type, state, ended_at, transcript_path, last_message,
                                    input_tokens, output_tokens, cache_read, cache_write,
                                    tool_calls)
             VALUES ($1, $2, $3, $4, $5, 'done', now(), $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (cc_session_id, cc_agent_id) WHERE cc_agent_id IS NOT NULL
               DO UPDATE SET state = 'done', ended_at = now(), updated_at = now()
             RETURNING {COLUMNS}"
        ))
        .bind(parent_agent_id)
        .bind(user_id)
        .bind(session_id)
        .bind(cc_agent_id)
        .bind(agent_type)
        .bind(report.transcript_path)
        .bind(report.last_message)
        .bind(report.input_tokens)
        .bind(report.output_tokens)
        .bind(report.cache_read)
        .bind(report.cache_write)
        .bind(report.tool_calls)
        .fetch_one(self.pool)
        .await?;
        Ok(Some(inserted))
    }

    /// Running subagents of `parent_agent_id` seen within `LIVE_WINDOW_SECS`.
    pub async fn live_count(&self, parent_agent_id: Uuid) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM subagents
              WHERE parent_agent_id = $1 AND state = 'running'
                AND updated_at > now() - make_interval(secs => $2)",
        )
        .bind(parent_agent_id)
        .bind(LIVE_WINDOW_SECS as f64)
        .fetch_one(self.pool)
        .await
    }

    /// Started subagents (running, or finished within `window_secs`) for
    /// `parent_agent_ids`, newest first.
    pub async fn list_recent(
        &self,
        parent_agent_ids: &[Uuid],
        window_secs: i64,
    ) -> Result<Vec<Subagent>, sqlx::Error> {
        sqlx::query_as::<_, Subagent>(&format!(
            "SELECT {COLUMNS} FROM subagents
              WHERE parent_agent_id = ANY($1) AND state <> 'pending'
                AND (state = 'running' OR updated_at > now() - make_interval(secs => $2))
              ORDER BY started_at DESC"
        ))
        .bind(parent_agent_ids)
        .bind(window_secs as f64)
        .fetch_all(self.pool)
        .await
    }
}

/// Whether a running row still counts as live (see `LIVE_WINDOW_SECS`).
pub fn is_live(sub: &Subagent, now: DateTime<Utc>) -> bool {
    sub.state == "running" && (now - sub.updated_at).num_seconds() < LIVE_WINDOW_SECS
}
