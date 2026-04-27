//! Regression for the lock-gantt projections (yggdrasil-161).

use chrono::{Duration as CDuration, Utc};
use std::time::Duration;
use ygg::tui::lock_gantt::{LockSpan, agent_color_index, span_columns, time_to_column};

#[test]
fn now_maps_to_rightmost_column() {
    let now = Utc::now();
    assert_eq!(time_to_column(now, now, Duration::from_secs(600), 100), 100);
}

#[test]
fn window_start_maps_to_leftmost_column() {
    let now = Utc::now();
    let start = now - CDuration::seconds(600);
    assert_eq!(time_to_column(start, now, Duration::from_secs(600), 100), 0);
}

#[test]
fn pre_window_instants_clamp_to_zero() {
    let now = Utc::now();
    let ancient = now - CDuration::seconds(3600);
    assert_eq!(
        time_to_column(ancient, now, Duration::from_secs(600), 100),
        0
    );
}

#[test]
fn future_instants_clamp_to_width() {
    let now = Utc::now();
    let future = now + CDuration::seconds(60);
    assert_eq!(
        time_to_column(future, now, Duration::from_secs(600), 100),
        100
    );
}

#[test]
fn agent_color_is_stable_across_calls() {
    let a = agent_color_index("alpha");
    let b = agent_color_index("alpha");
    assert_eq!(a, b);
}

#[test]
fn distinct_agents_get_distinct_colors() {
    let a = agent_color_index("alpha");
    let b = agent_color_index("beta");
    assert_ne!(a, b);
}

#[test]
fn span_columns_handles_live_lease() {
    let now = Utc::now();
    let span = LockSpan {
        resource: "src/db.rs".into(),
        holder_agent: "alpha".into(),
        acquired_at: now - CDuration::seconds(120),
        released_at: None,
    };
    let cols = span_columns(&span, now, Duration::from_secs(600), 100).unwrap();
    // 480/600 of the way across → ~80; live lease extends to right edge.
    assert!(cols.0 < cols.1);
    assert_eq!(cols.1, 100);
}

#[test]
fn span_columns_drops_pre_window_lease() {
    let now = Utc::now();
    let span = LockSpan {
        resource: "src/db.rs".into(),
        holder_agent: "alpha".into(),
        acquired_at: now - CDuration::seconds(2000),
        released_at: Some(now - CDuration::seconds(1500)),
    };
    assert!(span_columns(&span, now, Duration::from_secs(600), 100).is_none());
}
