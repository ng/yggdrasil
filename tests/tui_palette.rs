//! Regression for the command-palette state + ranking (yggdrasil-127).

use tui_input::Input;
use ygg::tui::palette::{Palette, default_actions, score};

#[test]
fn fresh_palette_is_closed() {
    let p = Palette::new(default_actions());
    assert!(!p.open);
}

#[test]
fn toggle_clears_input_when_closing() {
    let mut p = Palette::new(default_actions());
    p.toggle();
    p.input = Input::default().with_value("dash".into());
    p.toggle();
    assert!(!p.open);
    assert!(p.input.value().is_empty());
}

#[test]
fn empty_pattern_matches_every_registered_action() {
    let p = Palette::new(default_actions());
    let matches = p.matches();
    assert_eq!(matches.len(), default_actions().len());
}

#[test]
fn substring_pattern_filters_to_matching_actions() {
    let mut p = Palette::new(default_actions());
    p.toggle();
    p.input = Input::default().with_value("dash".into());
    let matches = p.matches();
    assert!(!matches.is_empty());
    assert!(matches.iter().any(|a| a.id == "switch:dashboard"));
    assert!(!matches.iter().any(|a| a.id == "switch:logs"));
}

#[test]
fn commit_returns_selected_action_id_and_closes() {
    let mut p = Palette::new(default_actions());
    p.toggle();
    p.input = Input::default().with_value("dash".into());
    let id = p.commit().expect("a match should commit");
    assert_eq!(id, "switch:dashboard");
    assert!(!p.open, "commit must close the palette");
}

#[test]
fn move_down_clamps_at_match_count() {
    let mut p = Palette::new(default_actions());
    p.toggle();
    p.input = Input::default().with_value("nonexistent-action-xyz".into());
    p.move_down();
    p.move_down();
    // Zero matches → selected stays at 0; commit returns None.
    assert_eq!(p.selected, 0);
    assert!(p.commit().is_none());
}

#[test]
fn score_prefers_prefix_over_substring() {
    let prefix = score("dash", "dashboard").unwrap();
    let inner = score("dash", "switch dashboard").unwrap();
    assert!(prefix > inner, "prefix matches must outrank substring");
}

#[test]
fn score_allows_subsequence_matches_for_typos() {
    // "swdb" should subsequence-match "switch to dashboard".
    let s = score("swdb", "switch to dashboard");
    assert!(
        s.is_some(),
        "subsequence path must find characters in order"
    );
}

#[test]
fn score_returns_none_when_chars_out_of_order() {
    // "bsh" can't sub-sequence "switch" (no 'b' before the 's').
    assert!(score("bsh", "switch").is_none());
}
