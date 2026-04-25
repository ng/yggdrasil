//! Scoped learnings — CodeRabbit-style rule capture. Key property:
//! retrieval is deterministic (SQL predicates on repo_id + file_glob +
//! rule_id) rather than vector-similarity. A learning scoped to
//! `terraform/*.tf` with rule_id `CKV_AWS_337` surfaces exactly when a
//! task touches `terraform/secrets.tf` and references that check — no
//! cosine, no threshold tuning, no false-positive noise.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
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
}

pub struct LearningRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> LearningRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        repo_id: Option<Uuid>,
        file_glob: Option<&str>,
        rule_id: Option<&str>,
        text: &str,
        context: Option<&str>,
        created_by: Option<Uuid>,
    ) -> Result<Learning, sqlx::Error> {
        sqlx::query_as::<_, Learning>(
            r#"INSERT INTO learnings (repo_id, file_glob, rule_id, text, context, created_by)
               VALUES ($1, $2, $3, $4, $5, $6)
               RETURNING learning_id, repo_id, file_glob, rule_id, text, context,
                         created_by, created_at, applied_count"#,
        )
        .bind(repo_id)
        .bind(file_glob)
        .bind(rule_id)
        .bind(text)
        .bind(context)
        .bind(created_by)
        .fetch_one(self.pool)
        .await
    }

    /// List learnings whose scope matches the filters. NULL filters act as
    /// "any"; a learning with `file_glob IS NULL` counts as "applies to any
    /// file in this repo" and is always included when `repo_id` matches
    /// (or when the learning itself is global).
    pub async fn list_matching(
        &self,
        repo_id: Option<Uuid>,
        file_path: Option<&str>,
        rule_id: Option<&str>,
    ) -> Result<Vec<Learning>, sqlx::Error> {
        // file_glob uses SQL LIKE after `*`-to-`%` + `?`-to-`_` substitution
        // (glob → LIKE). Done in the query via translate() to keep the
        // matcher in the DB. Restrict to scopes that either have no glob
        // (applies everywhere in the repo) or whose translated pattern
        // matches the provided file_path.
        sqlx::query_as::<_, Learning>(
            r#"
            SELECT learning_id, repo_id, file_glob, rule_id, text, context,
                   created_by, created_at, applied_count
            FROM learnings
            WHERE ($1::UUID IS NULL OR repo_id IS NULL OR repo_id = $1)
              AND ($2::TEXT IS NULL
                   OR file_glob IS NULL
                   OR $2 LIKE translate(file_glob, '*?', '%_'))
              AND ($3::TEXT IS NULL OR rule_id = $3)
            ORDER BY
              -- Most-specific first: rule_id + file_glob, then rule_id, then
              -- file_glob, then repo-only, then global.
              (rule_id IS NOT NULL)::int + (file_glob IS NOT NULL)::int DESC,
              created_at DESC
            "#,
        )
        .bind(repo_id)
        .bind(file_path)
        .bind(rule_id)
        .fetch_all(self.pool)
        .await
    }

    pub async fn increment_applied(&self, learning_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE learnings SET applied_count = applied_count + 1 WHERE learning_id = $1",
        )
        .bind(learning_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete(&self, learning_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM learnings WHERE learning_id = $1")
            .bind(learning_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }
}
