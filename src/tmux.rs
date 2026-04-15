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
            return Err(crate::YggError::Tmux("failed to create tmux session".into()));
        }
        Ok(())
    }

    /// Create a new window for a task with a split pane for status.
    pub async fn create_task_window(task_name: &str) -> Result<(), crate::YggError> {
        Self::ensure_session().await?;

        // Create new window
        let status = Command::new("tmux")
            .args(["new-window", "-t", SESSION_NAME, "-n", task_name])
            .status()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("new-window failed: {e}")))?;

        if !status.success() {
            return Err(crate::YggError::Tmux(format!(
                "failed to create window '{task_name}'"
            )));
        }

        // Split pane for status sidebar (20% right)
        Command::new("tmux")
            .args([
                "split-window",
                "-t",
                &format!("{SESSION_NAME}:{task_name}"),
                "-h",
                "-l",
                "20%",
            ])
            .status()
            .await
            .map_err(|e| crate::YggError::Tmux(format!("split-pane failed: {e}")))?;

        // Select the main pane (left)
        Command::new("tmux")
            .args([
                "select-pane",
                "-t",
                &format!("{SESSION_NAME}:{task_name}.0"),
            ])
            .status()
            .await
            .ok();

        Ok(())
    }

    /// Send keys to a specific pane.
    pub async fn send_keys(
        target: &str,
        keys: &str,
    ) -> Result<(), crate::YggError> {
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

    /// List all windows in the ygg session.
    pub async fn list_windows() -> Result<Vec<String>, crate::YggError> {
        let output = Command::new("tmux")
            .args([
                "list-windows",
                "-t",
                SESSION_NAME,
                "-F",
                "#{window_name}",
            ])
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
