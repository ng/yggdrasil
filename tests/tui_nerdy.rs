//! Regression for nerdy-pane formatters (yggdrasil-178).

use chrono::Duration;
use ygg::tui::nerdy::{NerdyView, humanize_age, humanize_bytes, humanize_count};

#[test]
fn fresh_view_is_unloaded() {
    let v = NerdyView::new();
    assert!(!v.stats.loaded);
}

#[test]
fn humanize_count_uses_k_and_m_suffixes() {
    assert_eq!(humanize_count(0), "0");
    assert_eq!(humanize_count(42), "42");
    assert_eq!(humanize_count(999), "999");
    assert_eq!(humanize_count(1_000), "1.0k");
    assert_eq!(humanize_count(1_500), "1.5k");
    assert_eq!(humanize_count(2_500_000), "2.5M");
}

#[test]
fn humanize_bytes_walks_through_units() {
    assert_eq!(humanize_bytes(0), "0 B");
    assert_eq!(humanize_bytes(512), "512 B");
    assert_eq!(humanize_bytes(2 * 1024), "2.0 KiB");
    assert_eq!(humanize_bytes(3 * 1024 * 1024), "3.0 MiB");
    assert_eq!(humanize_bytes(4 * 1024 * 1024 * 1024), "4.0 GiB");
}

#[test]
fn humanize_age_buckets_to_smallest_useful_unit() {
    assert_eq!(humanize_age(Duration::seconds(5)), "5s");
    assert_eq!(humanize_age(Duration::seconds(120)), "2m");
    assert_eq!(humanize_age(Duration::seconds(7200)), "2h");
    assert_eq!(humanize_age(Duration::seconds(86400 * 3)), "3d");
}

#[test]
fn humanize_age_handles_negative_durations_gracefully() {
    // Clock-skew can produce a negative duration; rather than
    // showing "-5s" we render "future" so the viewer notices.
    assert_eq!(humanize_age(Duration::seconds(-1)), "future");
}
