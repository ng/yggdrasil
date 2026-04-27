//! Regression for the run-river horizon-chart math (yggdrasil-159).

use ygg::tui::run_river::{
    BAND_GLYPHS, RiverSample, fold_to_horizon, glyph_for, render_strip, strip_max,
};

#[test]
fn glyph_for_zero_is_empty_band() {
    assert_eq!(glyph_for(0, 100), BAND_GLYPHS[0]);
}

#[test]
fn glyph_for_max_is_full_block() {
    assert_eq!(glyph_for(100, 100), '█');
}

#[test]
fn glyph_for_clamps_overflow() {
    assert_eq!(glyph_for(200, 100), '█');
}

#[test]
fn glyph_for_zero_max_returns_empty_band() {
    assert_eq!(glyph_for(50, 0), BAND_GLYPHS[0]);
}

#[test]
fn fold_below_half_only_lights_bottom_row() {
    // 25% intensity → bottom row carries it, top row stays empty.
    let (top, bottom) = fold_to_horizon(25, 100);
    assert_eq!(top, ' ');
    assert_ne!(bottom, ' ');
}

#[test]
fn fold_above_half_fills_bottom_and_overflows_top() {
    // 75% intensity → bottom is full ('█'), top picks up the upper half.
    let (top, bottom) = fold_to_horizon(75, 100);
    assert_eq!(bottom, '█');
    assert_ne!(top, ' ');
}

#[test]
fn strip_max_floors_at_one() {
    // All-zero strip should return 1, not 0 (avoids division by zero).
    let zeros = vec![RiverSample::default(); 5];
    assert_eq!(strip_max(&zeros), 1);
}

#[test]
fn render_strip_top_and_bottom_have_equal_length() {
    let samples = vec![
        RiverSample { tokens: 0 },
        RiverSample { tokens: 50 },
        RiverSample { tokens: 100 },
    ];
    let (top, bottom) = render_strip(&samples);
    assert_eq!(top.chars().count(), 3);
    assert_eq!(bottom.chars().count(), 3);
}
