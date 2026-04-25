use crate::config::AppConfig;
use sqlx::PgPool;

pub async fn execute_take_over(
    pool: &PgPool,
    config: &AppConfig,
    agent: &str,
) -> Result<(), anyhow::Error> {
    let snapshot = crate::interrupt::take_over(pool, config, agent).await?;
    println!(
        "Took over agent '{}' (was: {})",
        agent, snapshot.prior_state
    );
    println!(
        "  Head: {}",
        snapshot
            .head_node_id
            .map(|id| id.to_string())
            .unwrap_or("none".into())
    );
    println!("  Pressure: {} tokens", snapshot.context_tokens);
    println!("\nType in the agent's tmux pane. When done:");
    println!("  ygg interrupt hand-back {} \"what you did\"", agent);
    Ok(())
}

pub async fn execute_hand_back(
    pool: &PgPool,
    _config: &AppConfig,
    agent: &str,
    summary: &str,
) -> Result<(), anyhow::Error> {
    crate::interrupt::hand_back(pool, agent, summary).await?;
    println!("Handed back control to agent '{}'", agent);
    println!("  Summary recorded: {}", summary);
    Ok(())
}
