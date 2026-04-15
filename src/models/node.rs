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
        sqlx::query_as::<_, Node>(
            r#"
            WITH parent AS (
                SELECT ancestors, id FROM nodes WHERE id = $1
            )
            INSERT INTO nodes (parent_id, agent_id, kind, content, token_count, ancestors)
            SELECT $1, $2, $3::node_kind, $4, $5,
                   COALESCE(p.ancestors || p.id, '{}')
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
    pub async fn set_embedding(
        &self,
        node_id: Uuid,
        embedding: Vector,
    ) -> Result<(), sqlx::Error> {
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
            SELECT n.id, n.parent_id, n.agent_id, n.kind, n.content,
                   n.token_count, n.embedding, n.created_at, n.ancestors
            FROM nodes n
            WHERE n.id = $1
               OR n.id = ANY((SELECT ancestors FROM nodes WHERE id = $1))
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
            SELECT COALESCE(SUM(n.token_count::bigint), 0) AS total
            FROM nodes n
            WHERE n.id = $1
               OR n.id = ANY((SELECT ancestors FROM nodes WHERE id = $1))
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
        let kind_strings: Vec<String> = kinds.iter().map(|k| format!("{k:?}").to_lowercase()
            .replace("usermessage", "user_message")
            .replace("assistantmessage", "assistant_message")
            .replace("toolcall", "tool_call")
            .replace("toolresult", "tool_result")
            .replace("humanoverride", "human_override")
        ).collect();

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
        ).await
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
        ).await
    }

    /// Detect divergence: check if a node has children from different agents.
    pub async fn detect_divergence(&self, node_id: Uuid) -> Result<Vec<Vec<Node>>, sqlx::Error> {
        let children = self.get_children(node_id).await?;
        if children.len() < 2 { return Ok(vec![]); }

        // Group by agent_id
        let mut by_agent: std::collections::HashMap<Uuid, Vec<Node>> = std::collections::HashMap::new();
        for child in children {
            by_agent.entry(child.agent_id).or_default().push(child);
        }

        if by_agent.len() < 2 { return Ok(vec![]); }

        Ok(by_agent.into_values().collect())
    }
}
