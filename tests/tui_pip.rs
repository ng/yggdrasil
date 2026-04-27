//! Regression for the PiP transcript popup state machine (yggdrasil-167).

use std::thread::sleep;
use std::time::Duration;
use ygg::tui::pip::{HOVER_DWELL, PipState, TRANSCRIPT_CAP};

#[test]
fn fresh_state_is_closed_with_no_hover() {
    let s = PipState::default();
    assert!(!s.is_open());
    assert!(s.hover.is_none());
}

#[test]
fn cursor_on_starts_hover_timer() {
    let mut s = PipState::default();
    s.cursor_on("alpha");
    assert!(s.hover.is_some());
    assert!(!s.is_open(), "popup shouldn't open until dwell elapses");
}

#[test]
fn cursor_off_drops_hover_and_open_state() {
    let mut s = PipState::default();
    s.cursor_on("alpha");
    s.cursor_off();
    assert!(s.hover.is_none());
    assert!(!s.is_open());
}

#[test]
fn dwell_promotes_hover_into_open_popup() {
    let mut s = PipState::default();
    s.cursor_on("alpha");
    sleep(HOVER_DWELL + Duration::from_millis(50));
    let changed = s.tick();
    assert!(changed, "tick should report state change on promotion");
    assert!(s.is_open());
    assert_eq!(s.open.as_ref().unwrap().agent, "alpha");
    assert!(s.hover.is_none(), "hover timer cleared on open");
}

#[test]
fn cursor_on_different_agent_retargets() {
    let mut s = PipState::default();
    s.cursor_on("alpha");
    sleep(HOVER_DWELL + Duration::from_millis(20));
    s.tick();
    s.cursor_on("beta");
    // Re-hovering on a different agent closes the old popup
    // and starts a fresh dwell timer.
    assert!(!s.is_open());
    assert!(s.hover.is_some());
}

#[test]
fn push_line_respects_transcript_cap() {
    let mut s = PipState::default();
    s.cursor_on("alpha");
    sleep(HOVER_DWELL + Duration::from_millis(20));
    s.tick();
    for i in 0..(TRANSCRIPT_CAP + 50) {
        s.push_line(format!("line {i}"));
    }
    let open = s.open.as_ref().unwrap();
    assert_eq!(open.lines.len(), TRANSCRIPT_CAP);
    // Oldest line dropped — first remaining is line `50`.
    assert_eq!(open.lines.front().unwrap(), "line 50");
}

#[test]
fn close_resets_everything() {
    let mut s = PipState::default();
    s.cursor_on("alpha");
    sleep(HOVER_DWELL + Duration::from_millis(20));
    s.tick();
    s.close();
    assert!(!s.is_open());
    assert!(s.hover.is_none());
}
