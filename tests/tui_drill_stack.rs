//! Regression for the drill-stack state machine (yggdrasil-133).

use ygg::tui::drill_stack::{DrillStack, DrillStep, MAX_DEPTH};

fn step(label: &str) -> DrillStep {
    DrillStep {
        label: label.into(),
        payload: format!("payload-{label}"),
    }
}

#[test]
fn fresh_stack_is_empty_and_has_no_breadcrumb() {
    let s = DrillStack::default();
    assert_eq!(s.depth(), 0);
    assert!(s.is_empty());
    assert!(s.breadcrumb().is_empty());
}

#[test]
fn push_pop_round_trips_and_restores_payload() {
    let mut s = DrillStack::default();
    s.push(step("Tasks"));
    s.push(step("yggdrasil-42"));
    let popped = s.pop().unwrap();
    assert_eq!(popped.label, "yggdrasil-42");
    assert_eq!(popped.payload, "payload-yggdrasil-42");
    assert_eq!(s.depth(), 1);
}

#[test]
fn clear_returns_full_history_and_empties_stack() {
    let mut s = DrillStack::default();
    s.push(step("a"));
    s.push(step("b"));
    let dropped = s.clear();
    assert_eq!(dropped.len(), 2);
    assert_eq!(s.depth(), 0);
}

#[test]
fn breadcrumb_joins_with_chevrons() {
    let mut s = DrillStack::default();
    s.push(step("Tasks"));
    s.push(step("yggdrasil-42"));
    s.push(step("run #2"));
    assert_eq!(s.breadcrumb(), "Tasks › yggdrasil-42 › run #2");
}

#[test]
fn push_beyond_max_depth_rotates_oldest_out() {
    let mut s = DrillStack::default();
    for i in 0..(MAX_DEPTH + 5) {
        s.push(step(&format!("s{i}")));
    }
    assert_eq!(s.depth(), MAX_DEPTH);
    // Oldest entries rotated off the front; newest survives at the top.
    assert_eq!(s.stack.last().unwrap().label, format!("s{}", MAX_DEPTH + 4));
}
