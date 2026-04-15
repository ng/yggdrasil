use crate::config::AppConfig;
use crate::lock::LockManager;
use crate::models::agent::AgentRepo;

/// Handle `ygg lock acquire <resource>`
pub async fn acquire(pool: &sqlx::PgPool, config: &AppConfig, resource: &str, agent_name: &str) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let agent = agent_repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

    let lock_mgr = LockManager::new(pool, config.lock_ttl_secs);
    match lock_mgr.acquire(resource, agent.agent_id).await {
        Ok(lock) => {
            println!("Lock acquired: {} (expires {})", lock.resource_key, lock.expires_at);
        }
        Err(e) => {
            println!("Failed: {e}");
        }
    }
    Ok(())
}

/// Handle `ygg lock release <resource>`
pub async fn release(pool: &sqlx::PgPool, config: &AppConfig, resource: &str, agent_name: &str) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let agent = agent_repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

    let lock_mgr = LockManager::new(pool, config.lock_ttl_secs);
    lock_mgr.release(resource, agent.agent_id).await?;
    println!("Lock released: {resource}");
    Ok(())
}

/// Handle `ygg lock list`
pub async fn list(pool: &sqlx::PgPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    let lock_mgr = LockManager::new(pool, config.lock_ttl_secs);
    let locks = lock_mgr.list_all().await?;

    if locks.is_empty() {
        println!("No active locks.");
        return Ok(());
    }

    println!("{:<30} {:<38} {:<20}", "RESOURCE", "AGENT", "EXPIRES");
    for lock in &locks {
        println!(
            "{:<30} {:<38} {:<20}",
            lock.resource_key,
            lock.agent_id,
            lock.expires_at.format("%H:%M:%S"),
        );
    }
    println!("\n{} lock(s) active.", locks.len());
    Ok(())
}
