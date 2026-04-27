//! Retry-storm collapse + next-retry countdown (yggdrasil-147).
//!
//! When a task's last N attempts share the same fingerprint (= same
//! failure shape), the run-grid collapses them into a single
//! "12×, last err: <reason>" cell so retry storms don't smear the
//! pane. The countdown helper formats "next retry in 0:42" inline for
//! runs in `retrying` state with a known backoff deadline.

use chrono::{DateTime, Utc};
use std::time::Duration;

/// One historical attempt summary. Only the fields we group on plus
/// the human-readable reason are required; richer data lives on the
/// `task_runs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptSummary {
    pub attempt: i32,
    pub fingerprint: Option<String>,
    pub reason: String,
}

/// Threshold below which we don't collapse — needs at least N
/// repeated fingerprints in a row to count as a "storm". Three feels
/// right: two-attempt streaks happen on every retry, three is when
/// the eye starts to read it as repetition.
pub const COLLAPSE_THRESHOLD: usize = 3;

/// One collapsed group of attempts. `count` = number of consecutive
/// same-fingerprint attempts, `from` / `to` = inclusive attempt-number
/// bounds, `reason` = the most recent attempt's reason (used as the
/// label since the last failure is the most diagnostic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedGroup {
    pub count: usize,
    pub from: i32,
    pub to: i32,
    pub fingerprint: Option<String>,
    pub reason: String,
}

/// Walk attempts in attempt-DESC order (newest first); collapse runs
/// of identical fingerprint into single CollapsedGroup entries.
/// Singleton runs and runs shorter than COLLAPSE_THRESHOLD pass
/// through unchanged in a singleton group with `count = 1` so the
/// renderer's iteration doesn't have to special-case them.
pub fn collapse(attempts: &[AttemptSummary]) -> Vec<CollapsedGroup> {
    let mut out: Vec<CollapsedGroup> = Vec::new();
    let mut iter = attempts.iter().peekable();
    while let Some(first) = iter.next() {
        let mut group = vec![first];
        while let Some(next) = iter.peek() {
            if next.fingerprint == first.fingerprint && first.fingerprint.is_some() {
                group.push(iter.next().unwrap());
            } else {
                break;
            }
        }
        if group.len() < COLLAPSE_THRESHOLD {
            for a in group {
                out.push(CollapsedGroup {
                    count: 1,
                    from: a.attempt,
                    to: a.attempt,
                    fingerprint: a.fingerprint.clone(),
                    reason: a.reason.clone(),
                });
            }
        } else {
            let last = group.first().unwrap();
            let earliest = group.last().unwrap();
            out.push(CollapsedGroup {
                count: group.len(),
                from: earliest.attempt,
                to: last.attempt,
                fingerprint: first.fingerprint.clone(),
                reason: last.reason.clone(),
            });
        }
    }
    out
}

/// Format a CollapsedGroup as the inline cell label. Singletons
/// render as plain "att 7"; collapsed groups as "12×, reason".
pub fn group_label(group: &CollapsedGroup) -> String {
    if group.count == 1 {
        format!("att {}", group.from)
    } else {
        format!("{}×, {}", group.count, truncate(&group.reason, 24))
    }
}

/// Compute time remaining until the scheduled next retry. Returns
/// None if the deadline is in the past or unset; otherwise a Duration
/// the renderer formats via `format_countdown`.
pub fn time_until(deadline: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<Duration> {
    let d = deadline?;
    let secs = (d - now).num_seconds();
    if secs <= 0 {
        None
    } else {
        Some(Duration::from_secs(secs as u64))
    }
}

/// Format a duration as "M:SS" (mm:ss). Hours map to "Hh M" once the
/// run sleeps that long. Stays narrow so the inline cell budget holds.
pub fn format_countdown(d: Duration) -> String {
    let total = d.as_secs();
    if total >= 3600 {
        let h = total / 3600;
        let m = (total % 3600) / 60;
        format!("{h}h {m:02}m")
    } else {
        let m = total / 60;
        let s = total % 60;
        format!("{m}:{s:02}")
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
