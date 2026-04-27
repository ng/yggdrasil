//! Regression for the per-pane filter state machine (yggdrasil-128).

use tui_input::Input;
use ygg::tui::pane_filter::{FilterMode, PaneFilter};

#[test]
fn fresh_filter_is_off() {
    let f = PaneFilter::default();
    assert_eq!(f.mode, FilterMode::Off);
    assert!(f.matches("anything"));
}

#[test]
fn enter_then_cancel_returns_to_off() {
    let mut f = PaneFilter::default();
    f.enter();
    f.cancel();
    assert_eq!(f.mode, FilterMode::Off);
    assert!(f.matches("anything"));
}

#[test]
fn editing_with_substring_filters_live() {
    let mut f = PaneFilter::default();
    f.enter();
    f.input = Input::default().with_value("ggdr".into());
    assert!(f.matches("yggdrasil-42"));
    assert!(!f.matches("unrelated row"));
}

#[test]
fn matching_is_case_insensitive() {
    let mut f = PaneFilter::default();
    f.enter();
    f.input = Input::default().with_value("ALPHA".into());
    f.commit();
    assert!(f.matches("an Alpha task"));
    assert!(f.matches("alphabetical"));
    assert!(!f.matches("beta"));
}

#[test]
fn commit_with_empty_pattern_flips_back_to_off() {
    let mut f = PaneFilter::default();
    f.enter();
    f.commit();
    assert_eq!(f.mode, FilterMode::Off);
}

#[test]
fn commit_with_non_empty_pattern_activates() {
    let mut f = PaneFilter::default();
    f.enter();
    f.input = Input::default().with_value("x".into());
    f.commit();
    assert_eq!(f.mode, FilterMode::Active);
    assert!(f.matches("xeno"));
    assert!(!f.matches("yzzy"));
}

#[test]
fn label_distinguishes_editing_and_active() {
    let mut f = PaneFilter::default();
    assert!(f.label().is_none());
    f.enter();
    f.input = Input::default().with_value("foo".into());
    assert_eq!(f.label().as_deref(), Some("/foo"));
    f.commit();
    assert_eq!(f.label().as_deref(), Some("filter: foo"));
}
