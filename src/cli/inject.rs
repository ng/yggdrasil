use crate::config::AppConfig;
use crate::embed::Embedder;
use crate::models::agent::AgentRepo;
use crate::models::node::{NodeKind, NodeRepo};
use crate::lock::LockManager;

use tracing::{debug, info, warn};

/// Called by the UserPromptSubmit hook.
///
/// Flow:
///   1. If `prompt_text` is provided: embed it, write a UserMessage node, update head_node_id
///   2. Similarity-search across ALL agents for related past context
///   3. Surface high-similarity hits as `[ygg memory]` lines
///   4. Append active lock list
///
/// Returns nothing — output goes to stdout where the hook captures it for injection.
pub async fn execute(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    agent_name: &str,
    prompt_text: Option<&str>,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let node_repo = NodeRepo::new(pool);

    let agent = match agent_repo.get_by_name(agent_name).await? {
        Some(a) => a,
        None => {
            debug!("inject: agent '{}' not registered — skipping", agent_name);
            return Ok(());
        }
    };

    debug!(
        "inject: agent='{}' state={} tokens={} head_node={:?}",
        agent_name, agent.current_state, agent.context_tokens, agent.head_node_id
    );

    let mut output: Vec<String> = Vec::new();

    // ── context pressure warning ──────────────────────────────────────────────
    let pressure_pct = if config.context_limit_tokens > 0 {
        (agent.context_tokens as f64 / config.context_limit_tokens as f64 * 100.0) as u32
    } else {
        0
    };
    if pressure_pct > 75 {
        output.push(format!(
            "[ygg] Context pressure: {}% — digest will trigger at 100%",
            pressure_pct
        ));
    }

    // ── vector search ─────────────────────────────────────────────────────────
    if let Some(prompt) = prompt_text {
        let embedder = Embedder::default_ollama();
        let ollama_alive = embedder.health_check().await;
        debug!("inject: ollama health={}", ollama_alive);

        if ollama_alive {
            // Truncate prompt to ~1500 chars — all-minilm has a 256-token limit
            let query_text = if prompt.len() > 1500 { &prompt[..1500] } else { prompt };
            debug!("inject: embedding {} chars", query_text.len());

            match embedder.embed(query_text).await {
                Err(e) => warn!("inject: embed failed: {e}"),
                Ok(query_vec) => {
                    // Write this prompt as a UserMessage node and advance head_node_id
                    let node = node_repo.insert(
                        agent.head_node_id,
                        agent.agent_id,
                        NodeKind::UserMessage,
                        serde_json::json!({ "text": prompt }),
                        estimate_tokens(prompt),
                    ).await?;

                    node_repo.set_embedding(node.id, query_vec.clone()).await?;

                    let new_tokens = agent.context_tokens + node.token_count;
                    agent_repo.update_head(agent.agent_id, node.id, new_tokens).await?;

                    info!(
                        "inject: wrote node {} ({}tok), head advanced",
                        node.id, node.token_count
                    );

                    // Search ALL agents for similar past context
                    let hits = node_repo
                        .similarity_search_global(
                            &query_vec,
                            &[NodeKind::UserMessage, NodeKind::Directive, NodeKind::Digest],
                            8,
                            0.6, // cosine distance < 0.6 ≈ similarity > 0.4 for all-minilm
                        )
                        .await?;

                    debug!("inject: global search returned {} hits", hits.len());

                    for hit in &hits {
                        debug!(
                            "inject: hit agent={} dist={:.3} sim={:.3} kind={:?}",
                            hit.agent_name, hit.distance, hit.similarity(), hit.kind
                        );
                    }

                    // Exclude the node we just wrote (distance ≈ 0), surface the rest
                    let memories: Vec<_> = hits.iter()
                        .filter(|h| h.id != node.id && h.distance > 0.01)
                        .collect();

                    if !memories.is_empty() {
                        for hit in memories {
                            let age = format_age(hit.created_at);
                            let snippet = extract_snippet(&hit.content);
                            output.push(format!(
                                "[ygg memory | {} | {} | sim={:.0}%] {}",
                                hit.agent_name,
                                age,
                                hit.similarity() * 100.0,
                                snippet,
                            ));
                        }
                    }
                }
            }
        } else {
            debug!("inject: ollama unavailable — vector search skipped");
        }
    } else {
        debug!("inject: no prompt text — vector search skipped");
    }

    // ── lock status ───────────────────────────────────────────────────────────
    let lock_mgr = LockManager::new(pool, config.lock_ttl_secs);
    let locks = lock_mgr.list_agent_locks(agent.agent_id).await?;
    if !locks.is_empty() {
        let lock_list: Vec<String> = locks.iter().map(|l| l.resource_key.clone()).collect();
        output.push(format!("[ygg locks] holding: {}", lock_list.join(", ")));
    }

    if !output.is_empty() {
        println!("{}", output.join("\n"));
    }

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn estimate_tokens(text: &str) -> i32 {
    // Rough approximation: ~4 chars per token
    (text.len() / 4).max(1) as i32
}

fn extract_snippet(content: &serde_json::Value) -> String {
    // UserMessage nodes store { "text": "..." }
    // Directive nodes store { "directive": "..." }
    // Digest nodes store { "summary": "..." } or similar
    let text = content.get("text")
        .or_else(|| content.get("directive"))
        .or_else(|| content.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| content.as_str().unwrap_or("(no text)"));

    // Truncate to 120 chars
    if text.len() > 120 {
        format!("{}…", &text[..117])
    } else {
        text.to_string()
    }
}

fn format_age(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - ts).num_seconds();
    if secs < 60 { return format!("{secs}s ago"); }
    let mins = secs / 60;
    if mins < 60 { return format!("{mins}m ago"); }
    let hours = mins / 60;
    if hours < 24 { return format!("{hours}h ago"); }
    format!("{}d ago", hours / 24)
}
