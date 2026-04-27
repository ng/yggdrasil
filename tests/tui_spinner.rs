//! Regression for spinner + per-key pending tracker (yggdrasil-154).

use std::time::Duration;
use ygg::tui::spinner::{PendingSet, SPINNER_FRAMES, frame_at, pending_glyph_for_age};

#[test]
fn frame_cycle_repeats_at_modulo() {
    let a = frame_at(0);
    let b = frame_at(SPINNER_FRAMES.len() as u64);
    assert_eq!(a, b);
}

#[test]
fn distinct_phases_yield_distinct_glyphs() {
    let a = frame_at(0);
    let b = frame_at(1);
    assert_ne!(a, b);
}

#[test]
fn pending_set_round_trips_keys() {
    let mut p: PendingSet<u32> = PendingSet::new();
    p.mark(1);
    p.mark(2);
    assert!(p.is_pending(&1));
    assert!(p.is_pending(&2));
    p.clear(&1);
    assert!(!p.is_pending(&1));
    assert_eq!(p.len(), 1);
}

#[test]
fn clearing_unknown_key_is_noop() {
    let mut p: PendingSet<u32> = PendingSet::new();
    p.clear(&7);
    assert!(p.is_empty());
}

#[test]
fn glyph_escalates_to_slow_band_after_2_secs() {
    let fast = pending_glyph_for_age(0, Duration::from_millis(500));
    let slow = pending_glyph_for_age(0, Duration::from_secs(3));
    assert!(SPINNER_FRAMES.contains(&fast));
    assert!(!SPINNER_FRAMES.contains(&slow), "slow band must differ");
}

#[test]
fn glyph_switches_to_warning_after_8_secs() {
    let stuck = pending_glyph_for_age(0, Duration::from_secs(10));
    assert_eq!(stuck, '⚠');
}
