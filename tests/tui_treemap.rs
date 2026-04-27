//! Regression for the squarified-treemap layout (yggdrasil-163).

use ygg::tui::treemap::{TreemapItem, TreemapRect, priority_color, squarify};

fn item(label: &str, weight: u64, prio: f64) -> TreemapItem {
    TreemapItem {
        label: label.into(),
        weight,
        avg_priority: prio,
    }
}

fn bounds(width: u16, height: u16) -> TreemapRect {
    TreemapRect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

#[test]
fn empty_input_returns_no_cells() {
    let cells = squarify(&[], bounds(80, 24));
    assert!(cells.is_empty());
}

#[test]
fn zero_weight_items_are_skipped() {
    let items = vec![item("a", 0, 0.0), item("b", 5, 0.0)];
    let cells = squarify(&items, bounds(80, 24));
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].item.label, "b");
}

#[test]
fn single_item_fills_bounds() {
    let cells = squarify(&[item("only", 1, 0.0)], bounds(40, 10));
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].rect.width, 40);
    assert_eq!(cells[0].rect.height, 10);
}

#[test]
fn larger_weight_gets_larger_rect_area() {
    let items = vec![item("big", 10, 0.0), item("small", 1, 0.0)];
    let cells = squarify(&items, bounds(80, 24));
    let big = cells.iter().find(|c| c.item.label == "big").unwrap();
    let small = cells.iter().find(|c| c.item.label == "small").unwrap();
    let big_area = big.rect.width as u32 * big.rect.height as u32;
    let small_area = small.rect.width as u32 * small.rect.height as u32;
    assert!(big_area > small_area);
}

#[test]
fn rectangles_dont_overlap_or_escape_bounds() {
    // Two items at equal weight should partition the bounds cleanly.
    let cells = squarify(&[item("a", 1, 0.0), item("b", 1, 0.0)], bounds(40, 10));
    let total_area: u32 = cells
        .iter()
        .map(|c| c.rect.width as u32 * c.rect.height as u32)
        .sum();
    assert_eq!(total_area, 40 * 10);
    for c in &cells {
        assert!(c.rect.x + c.rect.width <= 40);
        assert!(c.rect.y + c.rect.height <= 10);
    }
}

#[test]
fn priority_zero_is_critical_red() {
    use ratatui::style::Color;
    assert_eq!(priority_color(0.0), Color::Indexed(196));
}

#[test]
fn priority_four_is_backlog_cool() {
    use ratatui::style::Color;
    assert_eq!(priority_color(4.0), Color::Indexed(67));
}
