//! `ygg remember "..."` — persist a durable directive node the similarity
//! retriever will surface across future sessions. Thin wrapper over the
//! existing `directive` node kind; the value add is the ergonomics and
//! tying the memory to the current repo + agent.

use crate::embed::Embedder;
use crate::models::agent::AgentRepo;
use crate::models::event::{EventKind, EventRepo};
use crate::models::node::{NodeKind, NodeRepo};
use crate::models::repo::RepoRepo;

/// Write a directive node under the given agent, scoped to the current repo
/// if one can be detected.
pub async fn remember(
    pool: &sqlx::PgPool,
    agent_name: &str,
    text: &str,
) -> Result<(), anyhow::Error> {
    let agent = AgentRepo::new(pool).register(agent_name).await?;

    // Best-effort repo detection; memory without a repo is still valid.
    let repo_id = match crate::cli::task_cmd::resolve_cwd_repo(pool).await {
        Ok(r) => Some(r.repo_id),
        Err(_) => None,
    };

    let token_count = (text.len() / 4).max(1) as i32;

    let node = NodeRepo::new(pool)
        .insert(
            agent.head_node_id,
            agent.agent_id,
            NodeKind::Directive,
            serde_json::json!({ "directive": text, "source": "ygg remember" }),
            token_count,
        )
        .await?;

    // Best-effort embed so similarity retrieval picks it up.
    let embedder = Embedder::default_ollama();
    if embedder.health_check().await {
        if let Ok((vec, _cached)) = embedder.embed_cached(pool, text).await {
            let _ = NodeRepo::new(pool).set_embedding(node.id, vec).await;
        }
    }

    // Link the node to the repo (if any) via a simple update — nodes now
    // carry an optional repo_id column after migration 20260417.
    if let Some(rid) = repo_id {
        let _ = sqlx::query("UPDATE nodes SET repo_id = $2 WHERE id = $1")
            .bind(node.id)
            .bind(rid)
            .execute(pool)
            .await;
    }

    // Defense in depth: redact the event snippet too. Node storage is
    // already redacted via NodeRepo::insert, but event snippets are a
    // separate path and must be sanitized independently.
    let (redacted_text, _) = crate::redaction::redact_str(text);
    let snippet = if redacted_text.len() > 80 {
        format!("{}…", &redacted_text[..80])
    } else {
        redacted_text.clone()
    };
    let _ = EventRepo::new(pool).emit(
        EventKind::Remembered,
        agent_name,
        Some(agent.agent_id),
        serde_json::json!({
            "snippet": snippet,
            "tokens": token_count,
            "repo_id": repo_id,
        }),
    ).await;

    println!("Remembered ({} tokens).", token_count);
    if let Some(rid) = repo_id {
        if let Some(repo) = RepoRepo::new(pool).get(rid).await? {
            println!("Scoped to repo: {} ({})", repo.name, repo.task_prefix);
        }
    } else {
        println!("(no repo context — available globally)");
    }
    Ok(())
}

pub async fn list(
    pool: &sqlx::PgPool,
    agent_name: Option<&str>,
    limit: i64,
) -> Result<(), anyhow::Error> {
    let rows: Vec<(chrono::DateTime<chrono::Utc>, String, serde_json::Value)> = if let Some(name) = agent_name {
        sqlx::query_as(
            r#"SELECT n.created_at, a.agent_name, n.content
               FROM nodes n JOIN agents a ON a.agent_id = n.agent_id
               WHERE n.kind = 'directive'
                 AND (n.content->>'source' = 'ygg remember' OR n.content ? 'directive')
                 AND a.agent_name = $1
               ORDER BY n.created_at DESC
               LIMIT $2"#,
        )
        .bind(name)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(
            r#"SELECT n.created_at, a.agent_name, n.content
               FROM nodes n JOIN agents a ON a.agent_id = n.agent_id
               WHERE n.kind = 'directive'
                 AND (n.content->>'source' = 'ygg remember' OR n.content ? 'directive')
               ORDER BY n.created_at DESC
               LIMIT $1"#,
        )
        .bind(limit)
        .fetch_all(pool)
        .await?
    };

    if rows.is_empty() {
        println!("No remembered directives.");
        return Ok(());
    }
    for (ts, agent, content) in &rows {
        let text = content.get("directive")
            .and_then(|v| v.as_str())
            .unwrap_or("(malformed directive)");
        let snippet = if text.len() > 100 { format!("{}…", &text[..99]) } else { text.to_string() };
        println!(
            "  [{}] {} — {}",
            ts.format("%Y-%m-%d %H:%M"),
            agent,
            snippet
        );
    }
    Ok(())
}
