//! Regression for the smooth gauge widget (yggdrasil-168).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;
use ygg::tui::smooth_gauge::{GaugePalette, PARTIAL_BLOCKS, SmoothGauge, partial_block};

#[test]
fn partial_block_handles_endpoints() {
    assert_eq!(partial_block(0.0), ' ');
    assert_eq!(partial_block(1.0), '█');
}

#[test]
fn partial_block_clamps_negatives_and_overflow() {
    assert_eq!(partial_block(-0.5), ' ');
    assert_eq!(partial_block(2.0), '█');
}

#[test]
fn partial_block_round_trips_each_eighth() {
    // Each 1/8 increment lands on a distinct glyph.
    let glyphs: Vec<char> = (0..=8).map(|n| partial_block(n as f64 / 8.0)).collect();
    assert_eq!(glyphs, PARTIAL_BLOCKS.to_vec());
}

#[test]
fn viridis_low_to_high_walks_dark_to_light() {
    let lo = GaugePalette::Viridis.color_at(0.0);
    let hi = GaugePalette::Viridis.color_at(1.0);
    assert_ne!(lo, hi, "endpoints should differ on the gradient");
}

#[test]
fn stoplight_partitions_into_three_bands() {
    assert_eq!(GaugePalette::Stoplight.color_at(0.10), Color::Green);
    assert_eq!(GaugePalette::Stoplight.color_at(0.50), Color::Yellow);
    assert_eq!(GaugePalette::Stoplight.color_at(0.90), Color::Red);
}

#[test]
fn smooth_gauge_renders_full_blocks_for_complete_fill() {
    let area = Rect::new(0, 0, 10, 1);
    let mut buf = Buffer::empty(area);
    SmoothGauge::new(1.0).render(area, &mut buf);
    for x in 0..10 {
        let cell = &buf[(x, 0)];
        assert_eq!(cell.symbol(), "█", "column {x} should be full");
    }
}

#[test]
fn smooth_gauge_renders_spaces_for_zero_fill() {
    let area = Rect::new(0, 0, 10, 1);
    let mut buf = Buffer::empty(area);
    SmoothGauge::new(0.0).render(area, &mut buf);
    for x in 0..10 {
        assert_eq!(buf[(x, 0)].symbol(), " ");
    }
}

#[test]
fn smooth_gauge_partial_fill_uses_one_eighth_block_in_tail_cell() {
    // 5 cells full + 1 cell at 0.5 = 5.5 / 10. That tail cell should
    // land on the 4th 1/8-block char (▌).
    let area = Rect::new(0, 0, 10, 1);
    let mut buf = Buffer::empty(area);
    SmoothGauge::new(0.55).render(area, &mut buf);
    // First five cells full.
    for x in 0..5 {
        assert_eq!(buf[(x, 0)].symbol(), "█", "column {x} should be full");
    }
    // Sixth cell carries the partial block.
    let tail = buf[(5, 0)].symbol().chars().next().unwrap();
    assert!(
        PARTIAL_BLOCKS.contains(&tail) && tail != ' ' && tail != '█',
        "tail glyph {tail:?} should be a partial block"
    );
}

#[test]
fn smooth_gauge_zero_area_is_safe() {
    // A widget rendered into a zero-height or zero-width Rect must not
    // panic — important since narrow-collapse sometimes hands gauges 0
    // rows on the way to a real layout pass.
    let area = Rect::new(0, 0, 0, 0);
    let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
    SmoothGauge::new(0.5).render(area, &mut buf);
}
