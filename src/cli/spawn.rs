use std::path::PathBuf;

use crate::config::AppConfig;
use crate::models::agent::AgentRepo;
use crate::tmux::TmuxManager;

/// Spawn a new Claude Code agent in a tmux window.
///
/// Each agent gets its own git worktree so concurrent agents never collide
/// on the shared working directory.
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

    if agent_name.is_empty() {
        anyhow::bail!(
            "agent name cannot be empty — provide --name or a task with alphanumeric characters"
        );
    }

    println!("Spawning agent '{agent_name}' for task: {task}");

    if !TmuxManager::is_available().await {
        anyhow::bail!("tmux is not available. Install tmux first.");
    }

    // Register agent in DB
    let agent_repo = AgentRepo::new(pool);
    let agent = agent_repo.register(&agent_name).await?;
    println!("  Agent ID: {}", agent.agent_id);

    // Create a git worktree so this agent has an isolated working copy.
    let worktree_path = create_worktree(&agent_name).await?;
    println!("  Worktree: {}", worktree_path.display());

    // Ensure tmux session + create task window. The helper returns pane
    // *ids* (tmux's stable @N references) rather than positional .0/.1
    // — the latter break under `pane-base-index 1` in .tmux.conf.
    TmuxManager::ensure_session().await?;
    let (left_pane, right_pane) = TmuxManager::create_task_window(&agent_name).await?;

    // cd into the worktree, then start Claude Code with the task as prompt.
    // YGG_SPAWN_PERMISSION_MODE overrides the default (bypassPermissions).
    // Accepted values: bypassPermissions, dontAsk, acceptEdits, default, plan.
    let cd_cmd = format!("cd '{}'", shell_escape(&worktree_path.to_string_lossy()));
    TmuxManager::send_keys(&left_pane, &cd_cmd).await?;
    let perm_mode =
        std::env::var("YGG_SPAWN_PERMISSION_MODE").unwrap_or_else(|_| "bypassPermissions".into());
    let claude_cmd = format!(
        "claude --dangerously-skip-permissions --permission-mode {} --name '{}' '{}'",
        shell_escape(&perm_mode),
        shell_escape(&agent_name),
        shell_escape(task),
    );
    TmuxManager::send_keys(&left_pane, &claude_cmd).await?;

    // Start the ygg observer in the sidebar (right) pane. It tails the
    // Claude Code JSONL transcript and ingests turns into the DAG.
    let ygg_bin = std::env::current_exe()
        .unwrap_or_else(|_| "ygg".into())
        .to_string_lossy()
        .to_string();
    let observe_cmd = format!(
        "'{}' observe --agent '{}'",
        shell_escape(&ygg_bin),
        shell_escape(&agent_name),
    );
    TmuxManager::send_keys(&right_pane, &observe_cmd).await?;

    println!("  Agent '{agent_name}' spawned in tmux");
    println!("  ├─ pane 0: Claude Code (agent)");
    println!("  ├─ pane 1: ygg observe (DAG ingest)");
    println!("  └─ worktree: {}", worktree_path.display());
    println!();
    println!("  tmux attach -t ygg");

    Ok(())
}

/// Create a git worktree for the agent under `.ygg/worktrees/<name>`.
/// Returns the absolute path to the new worktree.
async fn create_worktree(agent_name: &str) -> Result<PathBuf, anyhow::Error> {
    let branch_name = format!("ygg/{agent_name}");
    let worktree_dir = PathBuf::from(".ygg/worktrees").join(agent_name);

    if worktree_dir.exists() {
        // Reuse existing worktree (agent respawn / retry).
        return Ok(std::fs::canonicalize(&worktree_dir)?);
    }

    std::fs::create_dir_all(".ygg/worktrees")?;

    let output = tokio::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &branch_name,
            &worktree_dir.to_string_lossy(),
            "HEAD",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {stderr}");
    }

    Ok(std::fs::canonicalize(&worktree_dir)?)
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
