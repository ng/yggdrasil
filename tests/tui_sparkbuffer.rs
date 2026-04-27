//! Regression for the SparkBuffer ring + glyph encoding (yggdrasil-150).

use ygg::tui::app::SparkBuffer;

#[test]
fn empty_buffer_renders_spaces_to_keep_layout_stable() {
    let b = SparkBuffer::new(30);
    assert_eq!(b.glyphs(10), "          ");
}

#[test]
fn push_respects_cap() {
    let mut b = SparkBuffer::new(3);
    b.push(1);
    b.push(2);
    b.push(3);
    b.push(4);
    let v: Vec<u64> = b.samples.iter().copied().collect();
    assert_eq!(v, vec![2, 3, 4], "oldest sample drops at cap");
}

#[test]
fn glyphs_normalize_against_max() {
    let mut b = SparkBuffer::new(10);
    for v in [0, 4, 8] {
        b.push(v);
    }
    let g = b.glyphs(3);
    // 0 → ' ', 4/8 → middle band, 8/8 → '█'.
    let chars: Vec<char> = g.chars().collect();
    assert_eq!(chars[0], ' ');
    assert_eq!(chars[2], '█');
    assert_ne!(chars[1], ' ');
    assert_ne!(chars[1], '█');
}

#[test]
fn glyphs_pad_left_when_buffer_short() {
    let mut b = SparkBuffer::new(30);
    b.push(5);
    b.push(8);
    let g = b.glyphs(5);
    // 5 chars; last 2 carry data, leading 3 are padding spaces.
    assert_eq!(&g[..3], "   ");
}

#[test]
fn glyphs_request_zero_width_returns_empty() {
    let mut b = SparkBuffer::new(10);
    b.push(1);
    assert_eq!(b.glyphs(0), "");
}

#[test]
fn all_zero_buffer_renders_lowest_glyph() {
    let mut b = SparkBuffer::new(5);
    for _ in 0..5 {
        b.push(0);
    }
    let g = b.glyphs(5);
    // max = 1 (max=0 falls through), so every cell maps to ' ' (level 0).
    assert!(g.chars().all(|c| c == ' '));
}
