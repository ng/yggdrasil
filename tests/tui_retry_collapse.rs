//! Regression for retry-storm collapse + countdown formatting (yggdrasil-147).

use chrono::{Duration as CDuration, Utc};
use std::time::Duration;
use ygg::tui::retry_collapse::{
    AttemptSummary, COLLAPSE_THRESHOLD, collapse, format_countdown, group_label, time_until,
};

fn att(n: i32, fp: Option<&str>, reason: &str) -> AttemptSummary {
    AttemptSummary {
        attempt: n,
        fingerprint: fp.map(String::from),
        reason: reason.into(),
    }
}

#[test]
fn singleton_is_passed_through_uncollapsed() {
    let groups = collapse(&[att(1, Some("fp"), "ok")]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].count, 1);
}

#[test]
fn streak_below_threshold_does_not_collapse() {
    let groups = collapse(&[att(2, Some("fp"), "x"), att(1, Some("fp"), "x")]);
    assert_eq!(groups.len(), 2, "two of three is below threshold");
    assert!(groups.iter().all(|g| g.count == 1));
}

#[test]
fn streak_at_threshold_collapses_into_one_group() {
    let attempts: Vec<AttemptSummary> = (1..=COLLAPSE_THRESHOLD as i32)
        .rev()
        .map(|n| att(n, Some("fp"), "boom"))
        .collect();
    let groups = collapse(&attempts);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].count, COLLAPSE_THRESHOLD);
}

#[test]
fn missing_fingerprint_breaks_streaks() {
    // None fingerprints are never grouped — even if the reasons match.
    let groups = collapse(&[att(3, None, "x"), att(2, None, "x"), att(1, None, "x")]);
    assert_eq!(groups.len(), 3);
}

#[test]
fn group_label_includes_count_and_reason_for_collapsed() {
    let attempts = vec![
        att(5, Some("fp"), "tests failed"),
        att(4, Some("fp"), "tests failed"),
        att(3, Some("fp"), "tests failed"),
    ];
    let groups = collapse(&attempts);
    let label = group_label(&groups[0]);
    assert!(label.contains("3×"));
    assert!(label.contains("tests"));
}

#[test]
fn time_until_returns_none_when_deadline_past() {
    let now = Utc::now();
    let past = now - CDuration::seconds(60);
    assert!(time_until(Some(past), now).is_none());
    assert!(time_until(None, now).is_none());
}

#[test]
fn time_until_returns_remaining_when_in_future() {
    let now = Utc::now();
    let future = now + CDuration::seconds(42);
    let d = time_until(Some(future), now).unwrap();
    assert!(d.as_secs() <= 42);
    assert!(d.as_secs() >= 41);
}

#[test]
fn format_countdown_uses_mmss_under_one_hour() {
    let d = Duration::from_secs(42);
    assert_eq!(format_countdown(d), "0:42");
    let d = Duration::from_secs(125);
    assert_eq!(format_countdown(d), "2:05");
}

#[test]
fn format_countdown_uses_hours_at_or_above_one_hour() {
    let d = Duration::from_secs(3600 + 7 * 60);
    assert_eq!(format_countdown(d), "1h 07m");
}
