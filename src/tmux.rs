use std::process::Stdio;
use tokio::process::Command;

const SESSION_NAME: &str = "ygg";

/// Manages tmux sessions, windows, and panes for the orchestrator.
pub struct TmuxManager;

impl TmuxManager {
    /// Check if tmux is available.
    pub async fn is_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
    }

    /// Check if the ygg session exists.
    pub async fn session_exists() -> Result<bool, crate::YggError> {
        let status = Command::new("tmux")
            .args(["has-session", "-t", SESSION_NAME])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("tmux check failed: {e}")))?;

        Ok(status.success())
    }

    /// Create the ygg tmux session if it doesn't exist.
    pub async fn ensure_session() -> Result<(), crate::YggError> {
        if Self::session_exists().await? {
            return Ok(());
        }

        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", SESSION_NAME, "-n", "dashboard"])
            .status()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("create session failed: {e}")))?;

        if !status.success() {
            return Err(crate::YggError::Tmux(
                "failed to create tmux session".into(),
            ));
        }
        Ok(())
    }

    /// Create a new window for a task with a split pane for status.
    /// Returns `(left_pane_id, right_pane_id)` as tmux `@N` pane IDs.
    /// We use pane IDs rather than `<window>.<index>` because the user
    /// may have `pane-base-index 1` set in `.tmux.conf`, in which case
    /// `.0` doesn't exist and send-keys silently fails.
    pub async fn create_task_window(task_name: &str) -> Result<(String, String), crate::YggError> {
        Self::ensure_session().await?;

        // Create new window, capture the active pane's stable id.
        let out = Command::new("tmux")
            .args([
                "new-window",
                "-t",
                SESSION_NAME,
                "-n",
                task_name,
                "-P",
                "-F",
                "#{pane_id}",
            ])
            .output()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("new-window failed: {e}")))?;
        if !out.status.success() {
            return Err(crate::YggError::Tmux(format!(
                "failed to create window '{task_name}'"
            )));
        }
        let left_pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if left_pane.is_empty() {
            return Err(crate::YggError::Tmux(
                "tmux new-window returned no pane id".into(),
            ));
        }

        // Split that pane and capture the new pane's id too.
        let out = Command::new("tmux")
            .args([
                "split-window",
                "-t",
                &left_pane,
                "-h",
                "-l",
                "20%",
                "-P",
                "-F",
                "#{pane_id}",
            ])
            .output()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("split-pane failed: {e}")))?;
        if !out.status.success() {
            return Err(crate::YggError::Tmux("failed to split pane".into()));
        }
        let right_pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if right_pane.is_empty() {
            return Err(crate::YggError::Tmux(
                "tmux split-window returned no pane id".into(),
            ));
        }

        // Focus the main pane (left).
        Command::new("tmux")
            .args(["select-pane", "-t", &left_pane])
            .status()
            .await
            .ok();

        Ok((left_pane, right_pane))
    }

    /// Send keys to a specific pane.
    pub async fn send_keys(target: &str, keys: &str) -> Result<(), crate::YggError> {
        Command::new("tmux")
            .args(["send-keys", "-t", target, keys, "Enter"])
            .status()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("send-keys failed: {e}")))?;
        Ok(())
    }

    /// Select a window by name.
    pub async fn select_window(window_name: &str) -> Result<(), crate::YggError> {
        Command::new("tmux")
            .args([
                "select-window",
                "-t",
                &format!("{SESSION_NAME}:{window_name}"),
            ])
            .status()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("select-window failed: {e}")))?;
        Ok(())
    }

    /// Check if a window named `agent_name` exists in the ygg session.
    pub async fn has_agent_window(agent_name: &str) -> bool {
        Self::list_windows()
            .await
            .map(|ws| ws.iter().any(|w| w == agent_name))
            .unwrap_or(false)
    }

    /// List all windows in the ygg session.
    pub async fn list_windows() -> Result<Vec<String>, crate::YggError> {
        let output = Command::new("tmux")
            .args(["list-windows", "-t", SESSION_NAME, "-F", "#{window_name}"])
            .output()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("list-windows failed: {e}")))?;

        let names = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect();

        Ok(names)
    }
}
