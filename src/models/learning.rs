//! JSON model for scoped learnings — durable rule capture stored as OKF
//! documents (ADR 0019). Key property: retrieval is deterministic
//! (predicates on repo_id + file_glob + rule_id) rather than vector-similarity. A learning scoped to
//! `terraform/*.tf` with rule_id `CKV_AWS_337` surfaces exactly when a
//! task touches `terraform/secrets.tf` and references that check — no
//! cosine, no threshold tuning, no false-positive noise.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Learning {
    pub learning_id: Uuid,
    pub repo_id: Option<Uuid>,
    pub file_glob: Option<String>,
    pub rule_id: Option<String>,
    pub text: String,
    pub context: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub applied_count: i32,
    pub last_applied_at: Option<DateTime<Utc>>,
    pub scope_tags: serde_json::Value,
    /// `pending` | `active`. Only `active` learnings are ever surfaced
    /// (ADR 0017). `pending` rows exist but fire on nothing until promoted.
    pub status: String,
    /// `manual` (hand-written via `create`) | `proposed` (agent-authored via
    /// `propose`). Records how the learning entered the corpus.
    pub source: String,
    pub approved_at: Option<DateTime<Utc>>,
    pub approved_by: Option<Uuid>,
}
