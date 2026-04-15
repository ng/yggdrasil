use crate::config::AppConfig;
use crate::embed::Embedder;
use crate::models::agent::AgentRepo;
use crate::models::node::{NodeKind, NodeRepo};
use crate::salience::{Governor, SalienceConfig, ScoredDirective};

/// Called by the UserPromptSubmit hook.
/// Returns directives to inject near the attention cursor.
pub async fn execute(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let node_repo = NodeRepo::new(pool);

    let agent = match agent_repo.get_by_name(agent_name).await? {
        Some(a) => a,
        None => return Ok(()), // no agent registered yet, skip silently
    };

    // Get current context pressure
    let pressure_pct = if config.context_limit_tokens > 0 {
        (agent.context_tokens as f64 / config.context_limit_tokens as f64 * 100.0) as u32
    } else {
        0
    };

    let mut output = Vec::new();

    // Pressure warning
    if pressure_pct > 75 {
        output.push(format!(
            "[ygg] Context pressure: {}% — digest will trigger at {}%",
            pressure_pct,
            100
        ));
    }

    // Retrieve relevant directives via similarity search
    let embedder = Embedder::default_ollama();
    if embedder.health_check().await {
        // Use the agent's most recent node content as the query
        if let Some(head_id) = agent.head_node_id {
            let path = node_repo.get_ancestor_path(head_id).await?;
            if let Some(latest) = path.last() {
                let query_text = latest.content.to_string();
                if let Ok(query_vec) = embedder.embed(&query_text).await {
                    let results = node_repo
                        .similarity_search(
                            &query_vec,
                            agent.agent_id,
                            10,
                            &[NodeKind::Directive, NodeKind::Digest],
                        )
                        .await?;

                    // Apply salience governor
                    let mut governor = Governor::new(SalienceConfig::default());
                    let scored: Vec<ScoredDirective> = results
                        .iter()
                        .map(|node| {
                            let similarity = 0.8; // approximate — pgvector doesn't return scores with query_as
                            let token_distance = (agent.context_tokens - node.token_count).max(0) as usize;
                            let salience = governor.calculate_salience(similarity, token_distance);
                            ScoredDirective {
                                node_id: node.id,
                                content: node.content.to_string(),
                                token_count: node.token_count,
                                similarity,
                                token_distance,
                                salience,
                            }
                        })
                        .collect();

                    let directives = governor.govern(scored);
                    for d in &directives {
                        output.push(format!("[ygg directive] {}", d.content));
                    }
                }
            }
        }
    }

    // Lock status
    let lock_mgr = crate::lock::LockManager::new(pool, config.lock_ttl_secs);
    let locks = lock_mgr.list_agent_locks(agent.agent_id).await?;
    if !locks.is_empty() {
        let lock_list: Vec<String> = locks.iter().map(|l| l.resource_key.clone()).collect();
        output.push(format!("[ygg locks] holding: {}", lock_list.join(", ")));
    }

    // Print output — the hook captures this
    if !output.is_empty() {
        println!("{}", output.join("\n"));
    }

    Ok(())
}
