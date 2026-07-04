use std::collections::HashSet;

use tokio::process::Command;

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

impl TmuxProbeResult {
    pub fn permits_absence_evidence(&self) -> bool {
        matches!(
            self,
            Self::SessionMissing { .. } | Self::SessionPresent { .. }
        )
    }
}

pub async fn probe_session(session: &str) -> TmuxProbeResult {
    match Command::new("tmux").arg("-V").output().await {
        Ok(out) if out.status.success() => {}
        Ok(_) => return TmuxProbeResult::TmuxUnavailable,
        Err(e) => {
            return TmuxProbeResult::ProbeFailed {
                error: format!("tmux -V failed: {e}"),
            };
        }
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
        if stderr.contains("no server running") || stderr.contains("server exited") {
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
