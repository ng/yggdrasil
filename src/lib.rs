pub mod analytics;
pub mod bench;
pub mod blob;
pub mod classifier;
pub mod cli;
pub mod config;
pub mod db;
pub mod embed;
pub mod epoch;
pub mod hyde;
pub mod executor;
pub mod interrupt;
pub mod llm_digest;
pub mod lock;
pub mod models;
pub mod ollama;
pub mod pressure;
pub mod prompt;
pub mod redaction;
pub mod references;
pub mod salience;
pub mod scheduler;
pub mod scoring;
pub mod task_classify;
pub mod stats;
pub mod status;
pub mod tmux;
pub mod tui;
pub mod watcher;
pub mod worktree;

use sqlx::PgPool;
use uuid::Uuid;

/// Shared application state threaded through all subsystems.
pub struct AppState {
    pub pool: PgPool,
    pub agent_id: Uuid,
    pub config: config::AppConfig,
}

/// Unified error type for Ygg.
#[derive(Debug, thiserror::Error)]
pub enum YggError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),

    #[error("ollama: {0}")]
    Ollama(String),

    #[error("executor failed (exit {exit_code}): {stderr}")]
    Executor { exit_code: i32, stderr: String },

    #[error("invalid state transition: {from} -> {to}")]
    InvalidTransition { from: String, to: String },

    #[error("lock conflict: {0}")]
    Lock(#[from] lock::LockError),

    #[error("config: {0}")]
    Config(String),

    #[error("tmux: {0}")]
    Tmux(String),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
