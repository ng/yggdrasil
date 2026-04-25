use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "node_kind", rename_all = "snake_case")]
pub enum NodeKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    Digest,
    Directive,
    System,
    HumanOverride,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Node {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub agent_id: Uuid,
    pub kind: NodeKind,
    pub content: serde_json::Value,
    pub token_count: i32,
    #[serde(skip)]
    pub embedding: Option<Vector>,
    pub created_at: DateTime<Utc>,
    pub ancestors: Vec<Uuid>,
}

/// A node returned from a similarity search, with the actual cosine distance
/// from pgvector and the agent name joined in.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub agent_name: String,
    pub kind: NodeKind,
    pub content: serde_json::Value,
    pub token_count: i32,
    pub created_at: DateTime<Utc>,
    pub distance: f64, // cosine distance: 0 = identical, 1 = orthogonal
}

impl SearchHit {
    pub fn similarity(&self) -> f64 {
        (1.0 - self.distance).clamp(0.0, 1.0)
    }
}

pub struct NodeRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> NodeRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Insert a new node. Computes ancestors from parent atomically.
    pub async fn insert(
        &self,
        parent_id: Option<Uuid>,
        agent_id: Uuid,
        kind: NodeKind,
        content: serde_json::Value,
        token_count: i32,
    ) -> Result<Node, sqlx::Error> {
        // Secret redaction — run at this chokepoint so every write path
        // (inject, remember, digest, observe) gets it for free. Failure
        // mode is permissive: clean content passes through untouched.
        let (content, redacted) = crate::redaction::redact_json(content);
        if !redacted.is_clean() {
            // Emit audit event directly via raw SQL so we don't need an
            // EventRepo dependency on NodeRepo. Best-effort — a failed
            // audit insert must not block the node insert.
            let _ = sqlx::query(
                "INSERT INTO events (event_kind, agent_id, agent_name, payload, cc_session_id)
                 VALUES ('redaction_applied', $1, COALESCE((SELECT agent_name FROM agents WHERE agent_id = $1), ''), $2, $3)",
            )
            .bind(agent_id)
            .bind(serde_json::json!({
                "total": redacted.total,
                "kinds": redacted.counts,
                "node_kind": format!("{:?}", &kind).to_lowercase(),
            }))
            .bind(crate::models::event::cc_session_id())
            .execute(self.pool)
            .await;
        }
        sqlx::query_as::<_, Node>(
            r#"
            WITH parent AS (
                SELECT ancestors, id FROM nodes WHERE id = $1
            )
            INSERT INTO nodes (parent_id, agent_id, kind, content, token_count, ancestors)
            SELECT $1, $2, $3::node_kind, $4, $5,
                   CASE WHEN p.id IS NOT NULL THEN COALESCE(p.ancestors, '{}') || p.id ELSE '{}' END
            FROM (SELECT 1) AS dummy
            LEFT JOIN parent p ON TRUE
            RETURNING id, parent_id, agent_id, kind, content, token_count, embedding, created_at, ancestors
            "#,
        )
        .bind(parent_id)
        .bind(agent_id)
        .bind(&kind)
        .bind(&content)
        .bind(token_count)
        .fetch_one(self.pool)
        .await
    }

    /// Set embedding on an existing node.
    pub async fn set_embedding(&self, node_id: Uuid, embedding: Vector) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE nodes SET embedding = $1 WHERE id = $2")
            .bind(embedding)
            .bind(node_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Get all nodes in the path from root to this node, using ancestors.
    pub async fn get_ancestor_path(&self, node_id: Uuid) -> Result<Vec<Node>, sqlx::Error> {
        sqlx::query_as::<_, Node>(
            r#"
            WITH target AS (
                SELECT ancestors FROM nodes WHERE id = $1
            )
            SELECT n.id, n.parent_id, n.agent_id, n.kind, n.content,
                   n.token_count, n.embedding, n.created_at, n.ancestors
            FROM nodes n, target t
            WHERE n.id = $1
               OR n.id = ANY(t.ancestors)
            ORDER BY array_length(n.ancestors, 1) NULLS FIRST, n.created_at
            "#,
        )
        .bind(node_id)
        .fetch_all(self.pool)
        .await
    }

    /// Sum token_count for all nodes in the path from root to node_id.
    pub async fn calculate_path_tokens(&self, node_id: Uuid) -> Result<i64, sqlx::Error> {
        let row = sqlx::query(
            r#"
            WITH target AS (
                SELECT ancestors FROM nodes WHERE id = $1
            )
            SELECT COALESCE(SUM(n.token_count::bigint), 0)::bigint AS total
            FROM nodes n, target t
            WHERE n.id = $1
               OR n.id = ANY(t.ancestors)
            "#,
        )
        .bind(node_id)
        .fetch_one(self.pool)
        .await?;

        Ok(row.get::<i64, _>("total"))
    }

    /// Get direct children of a node (for divergence detection).
    pub async fn get_children(&self, node_id: Uuid) -> Result<Vec<Node>, sqlx::Error> {
        sqlx::query_as::<_, Node>(
            r#"
            SELECT id, parent_id, agent_id, kind, content, token_count,
                   embedding, created_at, ancestors
            FROM nodes
            WHERE parent_id = $1
            ORDER BY created_at
            "#,
        )
        .bind(node_id)
        .fetch_all(self.pool)
        .await
    }

    /// Similarity search: find top-k nodes nearest to query vector.
    /// Filters by specific node kinds to avoid retrieving irrelevant nodes.
    pub async fn similarity_search(
        &self,
        query_vec: &Vector,
        agent_id: Uuid,
        limit: i32,
        kinds: &[NodeKind],
    ) -> Result<Vec<Node>, sqlx::Error> {
        if kinds.is_empty() {
            // Fallback: search all embedded nodes
            return sqlx::query_as::<_, Node>(
                r#"
                SELECT id, parent_id, agent_id, kind, content, token_count,
                       embedding, created_at, ancestors
                FROM nodes
                WHERE agent_id = $1 AND embedding IS NOT NULL
                ORDER BY embedding <=> $2
                LIMIT $3
                "#,
            )
            .bind(agent_id)
            .bind(query_vec)
            .bind(limit as i64)
            .fetch_all(self.pool)
            .await;
        }

        // Filter by kind — cast the enum array to text for the ANY match
        let kind_strings: Vec<String> = kinds
            .iter()
            .map(|k| {
                format!("{k:?}")
                    .to_lowercase()
                    .replace("usermessage", "user_message")
                    .replace("assistantmessage", "assistant_message")
                    .replace("toolcall", "tool_call")
                    .replace("toolresult", "tool_result")
                    .replace("humanoverride", "human_override")
            })
            .collect();

        sqlx::query_as::<_, Node>(
            r#"
            SELECT id, parent_id, agent_id, kind, content, token_count,
                   embedding, created_at, ancestors
            FROM nodes
            WHERE agent_id = $1
              AND embedding IS NOT NULL
              AND kind::text = ANY($4)
            ORDER BY embedding <=> $2
            LIMIT $3
            "#,
        )
        .bind(agent_id)
        .bind(query_vec)
        .bind(limit as i64)
        .bind(&kind_strings)
        .fetch_all(self.pool)
        .await
    }

    /// Insert a directive node (reusable guidance injected into prompts).
    pub async fn insert_directive(
        &self,
        agent_id: Uuid,
        content: &str,
        parent_id: Option<Uuid>,
    ) -> Result<Node, sqlx::Error> {
        self.insert(
            parent_id,
            agent_id,
            NodeKind::Directive,
            serde_json::json!({ "directive": content }),
            crate::executor::estimate_tokens(content) as i32,
        )
        .await
    }

    /// Create a merge node that references results from multiple agents/branches.
    /// The merge node's parent is `primary_parent_id` (the branch being merged into),
    /// and its content includes references to all source nodes being merged.
    /// This provides fan-in semantics without changing the tree schema.
    pub async fn insert_merge(
        &self,
        primary_parent_id: Uuid,
        agent_id: Uuid,
        source_node_ids: &[Uuid],
        summary: &str,
    ) -> Result<Node, sqlx::Error> {
        let sources: Vec<String> = source_node_ids.iter().map(|id| id.to_string()).collect();
        self.insert(
            Some(primary_parent_id),
            agent_id,
            NodeKind::System,
            serde_json::json!({
                "merge": true,
                "sources": sources,
                "summary": summary,
            }),
            crate::executor::estimate_tokens(summary) as i32,
        )
        .await
    }

    /// Detect divergence: check if a node has children from different agents.
    pub async fn detect_divergence(&self, node_id: Uuid) -> Result<Vec<Vec<Node>>, sqlx::Error> {
        let children = self.get_children(node_id).await?;
        if children.len() < 2 {
            return Ok(vec![]);
        }

        // Group by agent_id
        let mut by_agent: std::collections::HashMap<Uuid, Vec<Node>> =
            std::collections::HashMap::new();
        for child in children {
            by_agent.entry(child.agent_id).or_default().push(child);
        }

        if by_agent.len() < 2 {
            return Ok(vec![]);
        }

        Ok(by_agent.into_values().collect())
    }

    /// Global similarity search across ALL agents — the cross-session memory query.
    /// Returns hits with actual cosine distances from pgvector (not hardcoded).
    /// `max_distance`: filter threshold (0.4 ≈ cosine similarity > 0.6).
    pub async fn similarity_search_global(
        &self,
        query_vec: &Vector,
        kinds: &[NodeKind],
        limit: i32,
        max_distance: f64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let kind_strings: Vec<String> = kinds
            .iter()
            .map(|k| {
                format!("{k:?}")
                    .to_lowercase()
                    .replace("usermessage", "user_message")
                    .replace("assistantmessage", "assistant_message")
                    .replace("toolcall", "tool_call")
                    .replace("toolresult", "tool_result")
                    .replace("humanoverride", "human_override")
            })
            .collect();

        let rows = sqlx::query(
            r#"
            SELECT n.id, n.agent_id, a.agent_name, n.kind::text AS kind_text,
                   n.content, n.token_count, n.created_at,
                   (n.embedding <=> $1)::float8 AS distance
            FROM nodes n
            JOIN agents a ON a.agent_id = n.agent_id
            WHERE n.embedding IS NOT NULL
              AND n.kind::text = ANY($2)
              AND (n.embedding <=> $1) < $3
            ORDER BY n.embedding <=> $1
            LIMIT $4
            "#,
        )
        .bind(query_vec)
        .bind(&kind_strings)
        .bind(max_distance)
        .bind(limit as i64)
        .fetch_all(self.pool)
        .await?;

        let hits = rows
            .into_iter()
            .map(|row| {
                use sqlx::Row;
                SearchHit {
                    id: row.get("id"),
                    agent_id: row.get("agent_id"),
                    agent_name: row.get("agent_name"),
                    kind: {
                        let k: String = row.get("kind_text");
                        match k.as_str() {
                            "user_message" => NodeKind::UserMessage,
                            "assistant_message" => NodeKind::AssistantMessage,
                            "tool_call" => NodeKind::ToolCall,
                            "tool_result" => NodeKind::ToolResult,
                            "digest" => NodeKind::Digest,
                            "directive" => NodeKind::Directive,
                            "human_override" => NodeKind::HumanOverride,
                            _ => NodeKind::System,
                        }
                    },
                    content: row.get("content"),
                    token_count: row.get("token_count"),
                    created_at: row.get("created_at"),
                    distance: row.get("distance"),
                }
            })
            .collect();

        Ok(hits)
    }

    /// Hybrid retrieval: union pgvector top-k with Postgres full-text
    /// top-k over the same query string, then merge via reciprocal rank
    /// fusion (RRF). RRF with k=60 is the retrieval-literature default.
    /// Each candidate gets score = sum over sources of 1/(60 + rank).
    /// Better recall than cosine-only, same SearchHit shape so callers
    /// don't have to care whether a hit came from lexical or semantic.
    pub async fn hybrid_search_global(
        &self,
        query_vec: &Vector,
        query_text: &str,
        kinds: &[NodeKind],
        limit: i32,
        max_distance: f64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let kind_strings: Vec<String> = kinds
            .iter()
            .map(|k| {
                format!("{k:?}")
                    .to_lowercase()
                    .replace("usermessage", "user_message")
                    .replace("assistantmessage", "assistant_message")
                    .replace("toolcall", "tool_call")
                    .replace("toolresult", "tool_result")
                    .replace("humanoverride", "human_override")
            })
            .collect();

        // Pull 2× limit per side so the post-RRF top-k has room to pick winners.
        let per_side = (limit as i64) * 2;
        let rrf_k = 60.0_f64;

        // Build a plain websearch_to_tsquery from the raw query — tolerant
        // of bad input, ignores stop words, good default for natural prose.
        let rows = sqlx::query(
            r#"
            WITH vec AS (
                SELECT n.id, (n.embedding <=> $1)::float8 AS dist,
                       row_number() OVER (ORDER BY n.embedding <=> $1) AS rank
                FROM nodes n
                WHERE n.embedding IS NOT NULL
                  AND n.kind::text = ANY($2)
                  AND (n.embedding <=> $1) < $3
                ORDER BY n.embedding <=> $1
                LIMIT $4
            ),
            lex AS (
                SELECT n.id,
                       row_number() OVER (ORDER BY ts_rank_cd(n.content_tsv, q) DESC) AS rank
                FROM nodes n, websearch_to_tsquery('english', $5) q
                WHERE n.kind::text = ANY($2)
                  AND n.content_tsv @@ q
                ORDER BY ts_rank_cd(n.content_tsv, q) DESC
                LIMIT $4
            ),
            fused AS (
                SELECT id,
                       SUM(score)::float8 AS fused_score,
                       MIN(dist)::float8 AS dist
                FROM (
                    SELECT id, 1.0 / ($6 + rank) AS score, dist FROM vec
                    UNION ALL
                    SELECT id, 1.0 / ($6 + rank) AS score, NULL::float8 AS dist FROM lex
                ) x
                GROUP BY id
            )
            SELECT n.id, n.agent_id, a.agent_name, n.kind::text AS kind_text,
                   n.content, n.token_count, n.created_at,
                   COALESCE(f.dist, 1.0)::float8 AS distance,
                   f.fused_score
            FROM fused f
            JOIN nodes  n ON n.id = f.id
            JOIN agents a ON a.agent_id = n.agent_id
            ORDER BY f.fused_score DESC
            LIMIT $7
            "#,
        )
        .bind(query_vec)
        .bind(&kind_strings)
        .bind(max_distance)
        .bind(per_side)
        .bind(query_text)
        .bind(rrf_k)
        .bind(limit as i64)
        .fetch_all(self.pool)
        .await?;

        let hits = rows
            .into_iter()
            .map(|row| {
                use sqlx::Row;
                SearchHit {
                    id: row.get("id"),
                    agent_id: row.get("agent_id"),
                    agent_name: row.get("agent_name"),
                    kind: {
                        let k: String = row.get("kind_text");
                        match k.as_str() {
                            "user_message" => NodeKind::UserMessage,
                            "assistant_message" => NodeKind::AssistantMessage,
                            "tool_call" => NodeKind::ToolCall,
                            "tool_result" => NodeKind::ToolResult,
                            "digest" => NodeKind::Digest,
                            "directive" => NodeKind::Directive,
                            "human_override" => NodeKind::HumanOverride,
                            _ => NodeKind::System,
                        }
                    },
                    content: row.get("content"),
                    token_count: row.get("token_count"),
                    created_at: row.get("created_at"),
                    distance: row.get("distance"),
                }
            })
            .collect();

        Ok(hits)
    }
}
