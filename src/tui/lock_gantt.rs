//! Lock swimlane gantt (yggdrasil-161). x = wall clock (last 10
//! min), y = resource key, bars colored by holder agent. Adjacent
//! same-resource bars in different colors = handoff (good); long
//! single bars = potential leak. The most directly diagnostic view
//! for the locking contract.
//!
//! This module ships:
//!   - `LockSpan` data shape (one lock lease over a time window)
//!   - `time_to_column` projection for windowed render
//!   - `agent_color_index` deterministic hash → 256-cube palette
//!
//! The pane that hosts the gantt + the SQL query that turns
//! `lock_events` into `LockSpan` rows lands incrementally; this PR
//! locks the math + colour mapping so the renderer is testable.

use ratatui::style::Color;
use std::time::Duration;

/// One lock lease — when it was acquired and (optionally) released.
/// `released_at = None` means the lease is still live; rendering
/// extends those bars to the right edge.
#[derive(Debug, Clone, PartialEq)]
pub struct LockSpan {
    pub resource: String,
    pub holder_agent: String,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
    pub released_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Default lookback window for the gantt (10 minutes). The
/// information-density research called this out as the sweet spot —
/// long enough to see queueing, short enough that bar widths stay
/// legible at typical pane widths.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(600);

/// Project a wall-clock instant onto the column index inside `width`
/// columns covering `window` ending at `now`. Earlier-than-window
/// instants clamp to 0; later-than-now instants clamp to `width`.
pub fn time_to_column(
    instant: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
    window: Duration,
    width: u16,
) -> u16 {
    if width == 0 {
        return 0;
    }
    let window_secs = window.as_secs() as i64;
    if window_secs == 0 {
        return 0;
    }
    let from_now = (now - instant).num_seconds();
    if from_now < 0 {
        return width;
    }
    if from_now > window_secs {
        return 0;
    }
    let frac = (window_secs - from_now) as f64 / window_secs as f64;
    let col = (frac * width as f64).round() as i64;
    col.clamp(0, width as i64) as u16
}

/// Deterministic agent → ANSI 256-cube color. Same agent always gets
/// the same colour across refreshes so the user pattern-matches on
/// hue rather than position. Indices 16..231 are the 6×6×6 cube;
/// reserve 16-21 (greys) so colours stay readable on either bg.
pub fn agent_color_index(agent: &str) -> Color {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    agent.hash(&mut hasher);
    let cube_offset = (hasher.finish() % 210) as u8 + 22; // 22..=231
    Color::Indexed(cube_offset)
}

/// Compute the (start_col, end_col) inclusive bounds for a span on a
/// gantt of `width` columns. Returns None if the span lies entirely
/// outside the window.
pub fn span_columns(
    span: &LockSpan,
    now: chrono::DateTime<chrono::Utc>,
    window: Duration,
    width: u16,
) -> Option<(u16, u16)> {
    if width == 0 {
        return None;
    }
    let cutoff = now - chrono::Duration::seconds(window.as_secs() as i64);
    let acquired = span.acquired_at;
    let released = span.released_at.unwrap_or(now);
    if released < cutoff {
        return None;
    }
    let start = time_to_column(acquired, now, window, width);
    let end = time_to_column(released, now, window, width);
    Some((start.min(end), start.max(end)))
}
