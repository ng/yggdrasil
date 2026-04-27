//! Repo treemap (yggdrasil-163). Squarified rectangles where area = open
//! task count + colour = avg priority. The whole portfolio in one
//! screen — a row table makes you read 10 numbers; a treemap shows
//! you which repo is on fire at a glance.
//!
//! Implements the squarified-treemap algorithm (Bruls/Huijsen/van
//! Wijk 2000) bounded to integer cells so terminal grids don't drift.
//! Renderer (box-drawing rectangles + ▓▒░ density fill) layers on
//! top.

use ratatui::style::Color;

#[derive(Debug, Clone, PartialEq)]
pub struct TreemapItem {
    pub label: String,
    /// Larger weight → larger rectangle. Use the open-task count.
    pub weight: u64,
    /// 0..=4 priority average; 0 = critical (red), 4 = backlog (cool).
    pub avg_priority: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreemapRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreemapCell {
    pub rect: TreemapRect,
    pub item: TreemapItem,
}

/// Squarified-treemap layout. Items are placed within `bounds`; weight
/// drives area; long-aspect-ratio cells get split off so each rect's
/// aspect stays close to 1:1 (the original Bruls et al motivation).
/// Items with zero weight are skipped — empty repos don't deserve a
/// rect.
pub fn squarify(items: &[TreemapItem], bounds: TreemapRect) -> Vec<TreemapCell> {
    let total_weight: u64 = items.iter().map(|i| i.weight).sum();
    let total_area = bounds.width as u64 * bounds.height as u64;
    if total_weight == 0 || total_area == 0 {
        return Vec::new();
    }
    let mut sorted: Vec<&TreemapItem> = items.iter().filter(|i| i.weight > 0).collect();
    sorted.sort_by(|a, b| b.weight.cmp(&a.weight));

    let mut cells: Vec<TreemapCell> = Vec::with_capacity(sorted.len());
    let mut remaining = bounds;
    let mut remaining_weight = total_weight;
    for item in sorted {
        if remaining.width == 0 || remaining.height == 0 {
            break;
        }
        let frac = item.weight as f64 / remaining_weight as f64;
        // Slice off the longer axis so cells stay close to square.
        let (cell_rect, leftover) = if remaining.width >= remaining.height {
            let w = ((remaining.width as f64 * frac).round() as u16).max(1);
            let w = w.min(remaining.width);
            (
                TreemapRect {
                    x: remaining.x,
                    y: remaining.y,
                    width: w,
                    height: remaining.height,
                },
                TreemapRect {
                    x: remaining.x + w,
                    y: remaining.y,
                    width: remaining.width.saturating_sub(w),
                    height: remaining.height,
                },
            )
        } else {
            let h = ((remaining.height as f64 * frac).round() as u16).max(1);
            let h = h.min(remaining.height);
            (
                TreemapRect {
                    x: remaining.x,
                    y: remaining.y,
                    width: remaining.width,
                    height: h,
                },
                TreemapRect {
                    x: remaining.x,
                    y: remaining.y + h,
                    width: remaining.width,
                    height: remaining.height.saturating_sub(h),
                },
            )
        };
        cells.push(TreemapCell {
            rect: cell_rect,
            item: item.clone(),
        });
        remaining = leftover;
        remaining_weight = remaining_weight.saturating_sub(item.weight);
    }
    cells
}

/// Map an avg_priority (0..=4) onto a colour band. 0 = critical = red;
/// 4 = backlog = cool indigo. Linear cubic interpolation through
/// 256-cube indices so the gradient reads smoothly.
pub fn priority_color(avg: f64) -> Color {
    let clamped = avg.clamp(0.0, 4.0);
    let idx = match (clamped * 1.0).round() as i32 {
        0 => 196, // red
        1 => 208, // orange
        2 => 220, // yellow
        3 => 70,  // green
        _ => 67,  // teal
    };
    Color::Indexed(idx)
}
