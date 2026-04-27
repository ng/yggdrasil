//! Regression for the calendar heatmap glyph mapping (yggdrasil-171).

use ratatui::style::Color;
use ygg::tui::calendar_heatmap::{HeatCell, cell_glyph};

#[test]
fn empty_cell_renders_dim_dotted_glyph() {
    let cell = HeatCell {
        total: 0,
        failed: 0,
    };
    let (glyph, color) = cell_glyph(&cell);
    assert_eq!(glyph, '░');
    assert_eq!(color, Color::DarkGray);
}

#[test]
fn zero_failure_rate_is_green() {
    let cell = HeatCell {
        total: 10,
        failed: 0,
    };
    let (glyph, color) = cell_glyph(&cell);
    assert_eq!(glyph, '█');
    assert_eq!(color, Color::Green);
}

#[test]
fn under_25_percent_failure_is_lightgreen() {
    let cell = HeatCell {
        total: 10,
        failed: 2,
    };
    let (_, color) = cell_glyph(&cell);
    assert_eq!(color, Color::LightGreen);
}

#[test]
fn high_failure_rate_is_red() {
    let cell = HeatCell {
        total: 10,
        failed: 9,
    };
    let (_, color) = cell_glyph(&cell);
    assert_eq!(color, Color::Red);
}

#[test]
fn fail_rate_handles_zero_total_without_panic() {
    let cell = HeatCell {
        total: 0,
        failed: 0,
    };
    assert_eq!(cell.fail_rate(), 0.0);
}

#[test]
fn fail_rate_is_proper_fraction() {
    let cell = HeatCell {
        total: 4,
        failed: 1,
    };
    assert!((cell.fail_rate() - 0.25).abs() < 1e-9);
}
