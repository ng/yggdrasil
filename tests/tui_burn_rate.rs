//! Regression for the burn-rate display (yggdrasil-148).

use ygg::tui::app::{cost_hidden, format_tokens_per_min, tokens_per_minute};

#[test]
fn tokens_per_minute_handles_top_of_hour() {
    // Bucket just opened — divide by 1, not 0.
    assert!((tokens_per_minute(60, 0) - 60.0).abs() < f64::EPSILON);
}

#[test]
fn tokens_per_minute_extrapolates_partial_hour() {
    // 600 tokens in 5 minutes → 120/min.
    assert!((tokens_per_minute(600, 5) - 120.0).abs() < f64::EPSILON);
}

#[test]
fn tokens_per_minute_caps_full_hour() {
    // 60_000 tokens in 60 minutes → 1000/min.
    assert!((tokens_per_minute(60_000, 60) - 1000.0).abs() < f64::EPSILON);
}

#[test]
fn formatter_humanizes_above_1k() {
    assert_eq!(format_tokens_per_min(1234.0), "1.2k tok/min");
    assert_eq!(format_tokens_per_min(999.0), "999 tok/min");
    assert_eq!(format_tokens_per_min(0.0), "0 tok/min");
}

#[test]
fn cost_hidden_respects_truthy_env() {
    unsafe { std::env::set_var("YGG_TUI_NO_COST", "1") };
    assert!(cost_hidden());
    unsafe { std::env::set_var("YGG_TUI_NO_COST", "true") };
    assert!(cost_hidden());
    unsafe { std::env::remove_var("YGG_TUI_NO_COST") };
    assert!(!cost_hidden());
}
