//! JSON model for `ygg remember` notes. Notes are OKF documents (ADR 0019);
//! this struct is the stable output contract, scoped to (repo_id, NULL=global).
//! Recent notes surface in `ygg prime` and `ygg remember --list` — never via
//! similarity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub memory_id: Uuid,
    pub repo_id: Option<Uuid>,
    pub text: String,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}
