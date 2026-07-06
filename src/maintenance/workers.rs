use uuid::Uuid;

use crate::maintenance::tmux_probe::TmuxProbeResult;
use crate::models::worker::Worker;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    Abandon {
        worker_id: Uuid,
        tmux_session: String,
        tmux_window: String,
        reason: &'static str,
    },
}

pub fn reconcile_from_probe(workers: &[Worker], probe: &TmuxProbeResult) -> Vec<ReconcileAction> {
    match probe {
        TmuxProbeResult::ProbeFailed { .. } | TmuxProbeResult::TmuxUnavailable => Vec::new(),
        TmuxProbeResult::SessionMissing { session } => workers
            .iter()
            .filter(|w| &w.tmux_session == session)
            .map(|w| ReconcileAction::Abandon {
                worker_id: w.worker_id,
                tmux_session: w.tmux_session.clone(),
                tmux_window: w.tmux_window.clone(),
                reason: "tmux session absent on reconciliation tick",
            })
            .collect(),
        TmuxProbeResult::SessionPresent { session, windows } => workers
            .iter()
            .filter(|w| &w.tmux_session == session && !windows.contains(&w.tmux_window))
            .map(|w| ReconcileAction::Abandon {
                worker_id: w.worker_id,
                tmux_session: w.tmux_session.clone(),
                tmux_window: w.tmux_window.clone(),
                reason: "tmux window absent on reconciliation tick",
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::models::worker::WorkerState;

    fn worker(session: &str, window: &str) -> Worker {
        Worker {
            worker_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            session_id: None,
            tmux_session: session.to_string(),
            tmux_window: window.to_string(),
            worktree_path: "/tmp/ygg-test-worker".to_string(),
            state: WorkerState::Running,
            started_at: Utc::now(),
            last_seen_at: Utc::now(),
            ended_at: None,
            exit_reason: None,
            branch_pushed: false,
            branch_merged: false,
            pr_url: None,
            delivery_checked_at: None,
            intent: None,
            window_reaped: false,
            worktree_removed: false,
        }
    }

    #[test]
    fn probe_failure_is_not_absence_evidence() {
        let workers = vec![worker("ygg", "a")];
        let actions = reconcile_from_probe(
            &workers,
            &TmuxProbeResult::ProbeFailed {
                error: "boom".into(),
            },
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn missing_window_abandons_only_that_worker() {
        let workers = vec![worker("ygg", "a"), worker("ygg", "b")];
        let actions = reconcile_from_probe(
            &workers,
            &TmuxProbeResult::SessionPresent {
                session: "ygg".into(),
                windows: HashSet::from(["a".to_string()]),
            },
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ReconcileAction::Abandon {
                worker_id: workers[1].worker_id,
                tmux_session: "ygg".into(),
                tmux_window: "b".into(),
                reason: "tmux window absent on reconciliation tick",
            }
        );
    }

    #[test]
    fn missing_session_abandons_workers_bound_to_that_session() {
        let workers = vec![worker("ygg", "a"), worker("other", "b")];
        let actions = reconcile_from_probe(
            &workers,
            &TmuxProbeResult::SessionMissing {
                session: "ygg".into(),
            },
        );
        assert_eq!(actions.len(), 1);
        let ReconcileAction::Abandon { tmux_window, .. } = &actions[0];
        assert_eq!(tmux_window, "a");
    }
}
