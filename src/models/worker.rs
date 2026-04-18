//! Worker: a spawned CC session executing a specific task in a worktree.
//! Differs from `sessions`: a session is any CC conversation under an
//! agent; a worker is specifically a task-spawned, tmux-hosted one that
//! the supervisor or TUI kicked off via `ygg plan run`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "worker_state", rename_all = "snake_case")]
pub enum WorkerState {
    Spawned,
    Running,
    Idle,
    NeedsAttention,
    Completed,
    Failed,
    Abandoned,
}

impl WorkerState {
    pub fn glyph_color(&self) -> (&'static str, &'static str) {
        match self {
            Self::Spawned        => ("◌", "dark_gray"),
            Self::Running        => ("▶", "green"),
            Self::Idle           => ("•", "gray"),
            Self::NeedsAttention => ("⚠", "yellow"),
            Self::Completed      => ("✓", "dark_gray"),
            Self::Failed         => ("✗", "red"),
            Self::Abandoned      => ("⊘", "dark_gray"),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Worker {
    pub worker_id:     Uuid,
    pub task_id:       Uuid,
    pub session_id:    Option<Uuid>,
    pub tmux_session:  String,
    pub tmux_window:   String,
    pub worktree_path: String,
    pub state:         WorkerState,
    pub started_at:    DateTime<Utc>,
    pub last_seen_at:  DateTime<Utc>,
    pub ended_at:      Option<DateTime<Utc>>,
    pub exit_reason:   Option<String>,
}

pub struct WorkerRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> WorkerRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self { Self { pool } }

    /// Register a freshly-spawned worker. Caller provides the tmux target
    /// strings it just created; we keep the exact window name so the
    /// observer can cross-check against `tmux list-windows`.
    pub async fn spawn(
        &self,
        task_id: Uuid,
        session_id: Option<Uuid>,
        tmux_session: &str,
        tmux_window: &str,
        worktree_path: &str,
    ) -> Result<Worker, sqlx::Error> {
        sqlx::query_as::<_, Worker>(
            r#"
            INSERT INTO workers
                (task_id, session_id, tmux_session, tmux_window, worktree_path, state)
            VALUES ($1, $2, $3, $4, $5, 'spawned')
            RETURNING worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason
            "#,
        )
        .bind(task_id)
        .bind(session_id)
        .bind(tmux_session)
        .bind(tmux_window)
        .bind(worktree_path)
        .fetch_one(self.pool)
        .await
    }

    pub async fn touch(&self, worker_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workers SET last_seen_at = now() WHERE worker_id = $1")
            .bind(worker_id).execute(self.pool).await?;
        Ok(())
    }

    pub async fn set_state(
        &self,
        worker_id: Uuid,
        state: WorkerState,
        exit_reason: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let terminal = matches!(state,
            WorkerState::Completed | WorkerState::Failed | WorkerState::Abandoned);
        sqlx::query(
            r#"
            UPDATE workers
               SET state = $2::worker_state,
                   last_seen_at = now(),
                   ended_at = CASE WHEN $3 AND ended_at IS NULL THEN now() ELSE ended_at END,
                   exit_reason = COALESCE($4, exit_reason)
             WHERE worker_id = $1
            "#,
        )
        .bind(worker_id).bind(&state).bind(terminal).bind(exit_reason)
        .execute(self.pool).await?;
        Ok(())
    }

    /// All workers whose tmux window should still exist. Used by the
    /// observer and the dashboard Workers panel.
    pub async fn list_live(&self) -> Result<Vec<Worker>, sqlx::Error> {
        sqlx::query_as::<_, Worker>(
            r#"SELECT worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason
                 FROM workers
                WHERE ended_at IS NULL
                ORDER BY started_at DESC"#,
        )
        .fetch_all(self.pool)
        .await
    }

    pub async fn get(&self, worker_id: Uuid) -> Result<Option<Worker>, sqlx::Error> {
        sqlx::query_as::<_, Worker>(
            r#"SELECT worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason
                 FROM workers WHERE worker_id = $1"#,
        )
        .bind(worker_id).fetch_optional(self.pool).await
    }
}
