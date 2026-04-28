use crate::config::AppConfig;
use crate::models::agent::AgentRepo;
use crate::tmux::TmuxManager;

/// Spawn a new Claude Code agent in a tmux window.
pub async fn execute(
    pool: &sqlx::PgPool,
    _config: &AppConfig,
    task: &str,
    name: Option<&str>,
) -> Result<(), anyhow::Error> {
    let agent_name = match name {
        Some(n) => n.to_string(),
        None => slugify(task),
    };

    println!("Spawning agent '{agent_name}' for task: {task}");

    if !TmuxManager::is_available().await {
        anyhow::bail!("tmux is not available. Install tmux first.");
    }

    // Register agent in DB
    let agent_repo = AgentRepo::new(pool);
    let agent = agent_repo.register(&agent_name).await?;
    println!("  Agent ID: {}", agent.agent_id);

    // Ensure tmux session + create task window. The helper returns pane
    // *ids* (tmux's stable @N references) rather than positional .0/.1
    // — the latter break under `pane-base-index 1` in .tmux.conf.
    TmuxManager::ensure_session().await?;
    let (left_pane, right_pane) = TmuxManager::create_task_window(&agent_name).await?;

    // Start Claude Code in the main (left) pane with the task as prompt.
    let claude_cmd = format!("claude --prompt '{}'", shell_escape(task),);
    TmuxManager::send_keys(&left_pane, &claude_cmd).await?;

    // Start the ygg observer in the sidebar (right) pane. It tails the
    // Claude Code JSONL transcript and ingests turns into the DAG.
    let ygg_bin = std::env::current_exe()
        .unwrap_or_else(|_| "ygg".into())
        .to_string_lossy()
        .to_string();
    let observe_cmd = format!("{} observe --agent {}", ygg_bin, shell_escape(&agent_name),);
    TmuxManager::send_keys(&right_pane, &observe_cmd).await?;

    println!("  Agent '{agent_name}' spawned in tmux");
    println!("  ├─ pane 0: Claude Code (agent)");
    println!("  └─ pane 1: ygg observe (DAG ingest)");
    println!();
    println!("  tmux attach -t ygg");

    Ok(())
}

fn slugify(task: &str) -> String {
    task.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
}

fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}
