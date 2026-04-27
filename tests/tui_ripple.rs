//! Regression for the cascade ripple queue (yggdrasil-166).

use ygg::tui::motion::{MAX_RIPPLES, RIPPLE_FRAMES, RippleQueue};

#[test]
fn empty_queue_has_nothing_active() {
    let q = RippleQueue::default();
    for d in 0..10 {
        assert!(!q.is_active_for_distance(d));
    }
}

#[test]
fn pushed_ripple_fires_one_distance_per_frame() {
    let mut q = RippleQueue::default();
    q.push(42);
    // First paint pass: distance 0 (origin) is hot.
    assert!(q.is_active_for_distance(0));
    assert!(!q.is_active_for_distance(1));
    q.tick_paint();
    assert!(q.is_active_for_distance(1));
    assert!(!q.is_active_for_distance(0));
}

#[test]
fn ripple_decays_after_max_frames() {
    let mut q = RippleQueue::default();
    q.push(1);
    for _ in 0..RIPPLE_FRAMES {
        q.tick_paint();
    }
    // Drained → no live ripples.
    assert!(q.ripples.is_empty());
    for d in 0..10 {
        assert!(!q.is_active_for_distance(d));
    }
}

#[test]
fn duplicate_origin_refreshes_rather_than_stacks() {
    let mut q = RippleQueue::default();
    q.push(7);
    q.tick_paint();
    q.push(7); // same origin
    assert_eq!(q.ripples.len(), 1, "duplicate origin must not stack");
    // Refreshed countdown resets to RIPPLE_FRAMES.
    assert_eq!(q.ripples[0].frames_remaining, RIPPLE_FRAMES);
}

#[test]
fn capacity_bounded_to_max_ripples() {
    let mut q = RippleQueue::default();
    for i in 0..(MAX_RIPPLES as u64 + 5) {
        q.push(i);
    }
    assert_eq!(q.ripples.len(), MAX_RIPPLES);
}

#[test]
fn motion_disabled_blocks_push() {
    unsafe { std::env::set_var("YGG_TUI_NO_MOTION", "1") };
    let mut q = RippleQueue::default();
    q.push(1);
    assert!(q.ripples.is_empty(), "disabled motion must not enqueue");
    unsafe { std::env::remove_var("YGG_TUI_NO_MOTION") };
}
