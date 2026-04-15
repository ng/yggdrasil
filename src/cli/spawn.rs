use crate::config::AppConfig;
use crate::tmux::TmuxManager;

/// Spawn a new agent in a tmux window.
pub async fn execute(
    _pool: &sqlx::PgPool,
    _config: &AppConfig,
    task: &str,
    name: Option<&str>,
) -> Result<(), anyhow::Error> {
    // Generate agent name from task if not provided
    let agent_name = match name {
        Some(n) => n.to_string(),
        None => slugify(task),
    };

    println!("Spawning agent '{agent_name}' for task: {task}");

    // Ensure tmux session exists
    if !TmuxManager::is_available().await {
        anyhow::bail!("tmux is not available. Install tmux first.");
    }

    TmuxManager::ensure_session().await?;

    // Create task window with status sidebar
    TmuxManager::create_task_window(&agent_name).await?;

    // Start the agent in the main pane (pane 0)
    let ygg_bin = std::env::current_exe()
        .unwrap_or_else(|_| "ygg".into())
        .to_string_lossy()
        .to_string();

    let run_cmd = format!(
        "{} run --name {} --task '{}'",
        ygg_bin,
        shell_escape(&agent_name),
        shell_escape(task),
    );

    TmuxManager::send_keys(
        &format!("ygg:{agent_name}.0"),
        &run_cmd,
    )
    .await?;

    // Start status watcher in the sidebar pane (pane 1)
    let status_cmd = format!("watch -n2 {} status --agent {}", ygg_bin, shell_escape(&agent_name));
    TmuxManager::send_keys(
        &format!("ygg:{agent_name}.1"),
        &status_cmd,
    )
    .await?;

    println!("Agent '{agent_name}' spawned in tmux window.");
    println!("  Attach: tmux attach -t ygg");
    println!("  Switch: Ctrl-b then select '{agent_name}'");

    Ok(())
}

/// Generate a slug from a task description for use as agent name.
fn slugify(task: &str) -> String {
    let slug: String = task
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();

    // Truncate and clean up
    slug.split('-')
        .filter(|s| !s.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
}

fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}
