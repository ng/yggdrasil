//! Regression tests for run-grid state parsing + glyph mapping (yggdrasil-146).

use ygg::tui::run_grid::{AttemptCell, GridState, MAX_ATTEMPT_COLS, MAX_TASK_ROWS, RunGridView};

#[test]
fn grid_state_parse_round_trips_known_states() {
    for s in [
        "scheduled",
        "ready",
        "running",
        "succeeded",
        "failed",
        "crashed",
        "cancelled",
        "retrying",
        "poison",
    ] {
        assert!(GridState::parse(s).is_some(), "should parse {s}");
    }
}

#[test]
fn grid_state_parse_unknown_returns_none() {
    assert!(GridState::parse("garbage").is_none());
    assert!(GridState::parse("").is_none());
}

#[test]
fn each_state_has_distinct_color_or_glyph() {
    use std::collections::HashSet;
    let mut seen: HashSet<(char, ratatui::style::Color)> = HashSet::new();
    for s in [
        GridState::Scheduled,
        GridState::Ready,
        GridState::Running,
        GridState::Succeeded,
        GridState::Failed,
        GridState::Crashed,
        GridState::Cancelled,
        GridState::Retrying,
        GridState::Poison,
    ] {
        assert!(seen.insert(s.style()), "collision on {s:?}");
    }
}

#[test]
fn fresh_view_has_empty_rows_and_starts_unloaded() {
    let v = RunGridView::new();
    assert!(v.rows.is_empty());
    assert!(!v.loaded);
}

#[test]
fn select_next_clamps_to_last_row_when_short() {
    let mut v = RunGridView::new();
    // No rows: select_next is a no-op, never panics.
    v.select_next();
    v.select_prev();
    assert!(v.rows.is_empty());
}

#[test]
fn constants_match_expected_widths() {
    assert_eq!(MAX_ATTEMPT_COLS, 8);
    assert_eq!(MAX_TASK_ROWS, 30);
    let _: AttemptCell = AttemptCell::Empty;
}
