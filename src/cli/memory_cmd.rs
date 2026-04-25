//! `ygg memory` — CRUD + search for the scoped memories table.

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::embed::Embedder;
use crate::models::memory::{MemoryRepo, MemoryScope};
use uuid::Uuid;

pub async fn create(
    pool: &sqlx::PgPool,
    agent_name: &str,
    scope: MemoryScope,
    text: &str,
) -> Result<(), anyhow::Error> {
    // Resolve the scope's required key.
    let (repo_id, cc_session) = match scope {
        MemoryScope::Global => (None, None),
        MemoryScope::Repo => {
            let r = resolve_cwd_repo(pool)
                .await
                .map_err(|e| anyhow::anyhow!("scope=repo requires a registered repo: {e}"))?;
            (Some(r.repo_id), None)
        }
        MemoryScope::Session => {
            let sid = crate::models::event::cc_session_id().ok_or_else(|| {
                anyhow::anyhow!("scope=session requires CLAUDE_SESSION_ID — run inside a CC hook")
            })?;
            (None, Some(sid))
        }
    };

    // Embed so the memory participates in similarity retrieval. Best-effort:
    // a failed embed still persists the text, just without the vector.
    let embedder = Embedder::default_ollama();
    let vec = if embedder.health_check().await {
        embedder.embed(text).await.ok()
    } else {
        None
    };

    let agent_id = find_agent_id(pool, agent_name).await;
    let created = MemoryRepo::new(pool)
        .create(
            scope.clone(),
            repo_id,
            cc_session.as_deref(),
            agent_id,
            agent_name,
            text,
            vec.as_ref(),
        )
        .await?;

    let scope_label = scope.as_str();
    let embed_note = if created.embedding.is_some() {
        ""
    } else {
        " (no embedding)"
    };
    println!(
        "[ygg memory] saved {} {}{}",
        scope_label,
        short(&text, 70),
        embed_note
    );
    Ok(())
}

pub async fn list(
    pool: &sqlx::PgPool,
    scope: Option<MemoryScope>,
    limit: i64,
) -> Result<(), anyhow::Error> {
    let (repo_id, cc_session) = scope_context(pool, scope.as_ref()).await;
    let rows = MemoryRepo::new(pool)
        .list(scope, repo_id, cc_session.as_deref(), limit)
        .await?;
    if rows.is_empty() {
        println!("(no memories match that scope)");
        return Ok(());
    }
    for m in rows {
        let pin = if m.pinned { "★" } else { " " };
        let age = humanize_age(m.created_at);
        println!(
            "{pin} [{}] {:<7} {:<10} {age}  {}",
            &m.memory_id.to_string()[..8],
            m.scope.as_str(),
            short(&m.agent_name, 10),
            short(&m.text, 80)
        );
    }
    Ok(())
}

pub async fn search(pool: &sqlx::PgPool, query: &str, limit: i64) -> Result<(), anyhow::Error> {
    let repo_id = resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id);
    let cc_session = crate::models::event::cc_session_id();

    let embedder = Embedder::default_ollama();
    if !embedder.health_check().await {
        anyhow::bail!("ollama unreachable — can't embed query");
    }
    let q_vec = embedder.embed(query).await?;
    let hits = MemoryRepo::new(pool)
        .search(&q_vec, repo_id, cc_session.as_deref(), limit, 0.5)
        .await?;
    if hits.is_empty() {
        println!("(no matches — try --limit or a different query)");
        return Ok(());
    }
    for h in hits {
        let pin = if h.memory.pinned { "★" } else { " " };
        let pct = (h.similarity * 100.0) as u32;
        println!(
            "{pin} [{}] {:>3}%  {:<7}  {}",
            &h.memory.memory_id.to_string()[..8],
            pct,
            h.memory.scope.as_str(),
            short(&h.memory.text, 90)
        );
    }
    Ok(())
}

pub async fn pin(pool: &sqlx::PgPool, memory_id: Uuid, pinned: bool) -> Result<(), anyhow::Error> {
    MemoryRepo::new(pool).set_pinned(memory_id, pinned).await?;
    println!(
        "[ygg memory] {} {}",
        if pinned { "pinned" } else { "unpinned" },
        memory_id
    );
    Ok(())
}

pub async fn expire(pool: &sqlx::PgPool, memory_id: Uuid, secs: i64) -> Result<(), anyhow::Error> {
    MemoryRepo::new(pool).expire_in(memory_id, secs).await?;
    println!("[ygg memory] expire_at set to now+{secs}s");
    Ok(())
}

pub async fn delete(pool: &sqlx::PgPool, memory_id: Uuid) -> Result<(), anyhow::Error> {
    MemoryRepo::new(pool).delete(memory_id).await?;
    println!("[ygg memory] deleted {memory_id}");
    Ok(())
}

async fn scope_context(
    pool: &sqlx::PgPool,
    scope: Option<&MemoryScope>,
) -> (Option<Uuid>, Option<String>) {
    match scope {
        Some(MemoryScope::Repo) => {
            let r = resolve_cwd_repo(pool).await.ok();
            (r.map(|r| r.repo_id), None)
        }
        Some(MemoryScope::Session) => (None, crate::models::event::cc_session_id()),
        _ => (None, None),
    }
}

async fn find_agent_id(pool: &sqlx::PgPool, agent_name: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT agent_id FROM agents WHERE agent_name = $1")
        .bind(agent_name)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

fn humanize_age(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - ts).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}
