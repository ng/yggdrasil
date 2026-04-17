//! Memories: pinned, scoped, embedded notes — retrieved by similarity
//! alongside transcript nodes, but with explicit lifecycle (scope, pin,
//! expire). Three scopes:
//!
//!   - global  : surfaces in every session, every repo
//!   - repo    : only when retrieving for tasks in that repo
//!   - session : only within a specific Claude Code session
//!
//! Separate from `nodes` so the retriever can score them independently
//! and users can list/pin/expire them without slogging through the DAG.

use chrono::{DateTime, Duration, Utc};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "memory_scope", rename_all = "snake_case")]
pub enum MemoryScope {
    Global,
    Repo,
    Session,
}

impl MemoryScope {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Global => "global", Self::Repo => "repo", Self::Session => "session" }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "global" | "g" => Some(Self::Global),
            "repo" | "r" => Some(Self::Repo),
            "session" | "s" => Some(Self::Session),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Memory {
    pub memory_id: Uuid,
    pub scope: MemoryScope,
    pub repo_id: Option<Uuid>,
    pub cc_session_id: Option<String>,
    pub agent_id: Option<Uuid>,
    pub agent_name: String,
    pub text: String,
    #[sqlx(default)]
    pub embedding: Option<Vector>,
    pub pinned: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub memory: Memory,
    pub similarity: f64,
}

pub struct MemoryRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> MemoryRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self { Self { pool } }

    pub async fn create(
        &self,
        scope: MemoryScope,
        repo_id: Option<Uuid>,
        cc_session_id: Option<&str>,
        agent_id: Option<Uuid>,
        agent_name: &str,
        text: &str,
        embedding: Option<&Vector>,
    ) -> Result<Memory, sqlx::Error> {
        sqlx::query_as::<_, Memory>(
            r#"
            INSERT INTO memories
                (scope, repo_id, cc_session_id, agent_id, agent_name, text, embedding)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING memory_id, scope, repo_id, cc_session_id, agent_id,
                      agent_name, text, embedding, pinned, expires_at,
                      created_at, updated_at
            "#,
        )
        .bind(scope)
        .bind(repo_id)
        .bind(cc_session_id)
        .bind(agent_id)
        .bind(agent_name)
        .bind(text)
        .bind(embedding)
        .fetch_one(self.pool)
        .await
    }

    /// List memories filtered by optional scope. Unexpired unless
    /// `include_expired=true`. Pinned always surfaces first.
    pub async fn list(
        &self,
        scope: Option<MemoryScope>,
        repo_id: Option<Uuid>,
        cc_session_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Memory>, sqlx::Error> {
        sqlx::query_as::<_, Memory>(
            r#"
            SELECT memory_id, scope, repo_id, cc_session_id, agent_id,
                   agent_name, text, embedding, pinned, expires_at,
                   created_at, updated_at
            FROM memories
            WHERE (expires_at IS NULL OR expires_at > now())
              AND ($1::memory_scope IS NULL OR scope = $1)
              AND ($2::uuid IS NULL OR repo_id = $2)
              AND ($3::text IS NULL OR cc_session_id = $3)
            ORDER BY pinned DESC, created_at DESC
            LIMIT $4
            "#,
        )
        .bind(scope)
        .bind(repo_id)
        .bind(cc_session_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Semantic search across memories visible in the given scope context.
    /// A memory is visible when its scope matches or is broader:
    ///   - global memories always visible
    ///   - repo memories visible when `repo_id` matches
    ///   - session memories visible when `cc_session_id` matches
    pub async fn search(
        &self,
        query_vec: &Vector,
        repo_id: Option<Uuid>,
        cc_session_id: Option<&str>,
        limit: i64,
        max_distance: f64,
    ) -> Result<Vec<MemoryHit>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT memory_id, scope::text AS scope_text, repo_id, cc_session_id,
                   agent_id, agent_name, text, pinned, expires_at,
                   created_at, updated_at,
                   (embedding <=> $1)::float8 AS distance
            FROM memories
            WHERE embedding IS NOT NULL
              AND (expires_at IS NULL OR expires_at > now())
              AND (
                scope = 'global'
                OR (scope = 'repo'    AND repo_id = $2)
                OR (scope = 'session' AND cc_session_id = $3)
              )
              AND (embedding <=> $1) < $4
            ORDER BY pinned DESC, embedding <=> $1
            LIMIT $5
            "#,
        )
        .bind(query_vec)
        .bind(repo_id)
        .bind(cc_session_id)
        .bind(max_distance)
        .bind(limit)
        .fetch_all(self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| {
            let scope_text: String = r.get("scope_text");
            let scope = MemoryScope::parse(&scope_text).unwrap_or(MemoryScope::Global);
            let distance: f64 = r.get("distance");
            MemoryHit {
                memory: Memory {
                    memory_id: r.get("memory_id"),
                    scope,
                    repo_id: r.get("repo_id"),
                    cc_session_id: r.get("cc_session_id"),
                    agent_id: r.get("agent_id"),
                    agent_name: r.get("agent_name"),
                    text: r.get("text"),
                    embedding: None,
                    pinned: r.get("pinned"),
                    expires_at: r.get("expires_at"),
                    created_at: r.get("created_at"),
                    updated_at: r.get("updated_at"),
                },
                similarity: (1.0 - distance).clamp(0.0, 1.0),
            }
        }).collect())
    }

    pub async fn set_pinned(&self, memory_id: Uuid, pinned: bool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE memories SET pinned = $2, updated_at = now() WHERE memory_id = $1"
        )
        .bind(memory_id).bind(pinned)
        .execute(self.pool).await?;
        Ok(())
    }

    pub async fn expire_in(&self, memory_id: Uuid, secs: i64) -> Result<(), sqlx::Error> {
        let when = Utc::now() + Duration::seconds(secs);
        sqlx::query(
            "UPDATE memories SET expires_at = $2, updated_at = now() WHERE memory_id = $1"
        )
        .bind(memory_id).bind(when)
        .execute(self.pool).await?;
        Ok(())
    }

    pub async fn delete(&self, memory_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM memories WHERE memory_id = $1")
            .bind(memory_id).execute(self.pool).await?;
        Ok(())
    }
}
