use crate::lock::LockManager;
use crate::models::agent::AgentRepo;

/// Handle `ygg status [--agent <name>]`
pub async fn execute(pool: &sqlx::PgPool, agent_name: Option<&str>) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);

    if let Some(name) = agent_name {
        // Single agent status
        let agent = agent_repo
            .get_by_name(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", name))?;

        println!("Agent: {}", agent.agent_name);
        println!("  ID:       {}", agent.agent_id);
        println!("  State:    {}", agent.current_state);
        println!("  Pressure: {} tokens", agent.context_tokens);
        println!(
            "  Head:     {}",
            agent
                .head_node_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".into())
        );
        println!(
            "  Digest:   {}",
            agent
                .digest_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".into())
        );
        println!("  Updated:  {}", agent.updated_at);

        let lock_mgr = LockManager::new(pool, 300);
        let locks = lock_mgr.list_agent_locks(agent.agent_id).await?;
        if locks.is_empty() {
            println!("  Locks:    none");
        } else {
            for lock in &locks {
                println!(
                    "  Lock:     {} (expires {})",
                    lock.resource_key,
                    lock.expires_at.format("%H:%M:%S")
                );
            }
        }
    } else {
        // All agents
        let agents = agent_repo.list().await?;

        if agents.is_empty() {
            println!("No agents registered.");
            return Ok(());
        }

        println!(
            "{:<20} {:<15} {:<12} {:<20}",
            "NAME", "STATE", "PRESSURE", "UPDATED"
        );
        for agent in &agents {
            println!(
                "{:<20} {:<15} {:<12} {:<20}",
                agent.agent_name,
                agent.current_state.to_string(),
                format!("{} tok", agent.context_tokens),
                agent.updated_at.format("%H:%M:%S"),
            );
        }
        println!("\n{} agent(s).", agents.len());
    }

    Ok(())
}
