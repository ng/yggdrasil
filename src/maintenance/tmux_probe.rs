use std::collections::HashSet;

use tokio::process::Command;
use tokio::sync::OnceCell;

static TMUX_AVAILABLE: OnceCell<()> = OnceCell::const_new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxProbeResult {
    ProbeFailed {
        error: String,
    },
    TmuxUnavailable,
    SessionMissing {
        session: String,
    },
    SessionPresent {
        session: String,
        windows: HashSet<String>,
    },
}

async fn tmux_binary_available() -> bool {
    if TMUX_AVAILABLE.get().is_some() {
        return true;
    }

    let available = matches!(
        Command::new("tmux").arg("-V").output().await,
        Ok(out) if out.status.success()
    );
    if available {
        let _ = TMUX_AVAILABLE.set(());
    }
    available
}

pub async fn probe_session(session: &str) -> TmuxProbeResult {
    if !tmux_binary_available().await {
        return TmuxProbeResult::TmuxUnavailable;
    }

    let has = match Command::new("tmux")
        .args(["has-session", "-t", session])
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            return TmuxProbeResult::ProbeFailed {
                error: format!("tmux has-session failed: {e}"),
            };
        }
    };

    if !has.status.success() {
        let stderr = String::from_utf8_lossy(&has.stderr).to_lowercase();
        if stderr.contains("no server running")
            || stderr.contains("server exited")
            || stderr.contains("error connecting to")
            || stderr.contains("no such file or directory")
        {
            return TmuxProbeResult::TmuxUnavailable;
        }
        return TmuxProbeResult::SessionMissing {
            session: session.to_string(),
        };
    }

    let listed = match Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_name}"])
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            return TmuxProbeResult::ProbeFailed {
                error: format!("tmux list-windows failed: {e}"),
            };
        }
    };

    if !listed.status.success() {
        return TmuxProbeResult::ProbeFailed {
            error: format!(
                "tmux list-windows failed after has-session: {}",
                String::from_utf8_lossy(&listed.stderr).trim()
            ),
        };
    }

    let windows = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    TmuxProbeResult::SessionPresent {
        session: session.to_string(),
        windows,
    }
}
