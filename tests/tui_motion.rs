//! Regression for the motion-vocabulary primitives (yggdrasil-165 + 169).

use ratatui::style::{Color, Modifier, Style};
use ygg::tui::motion::{
    FLASH_FRAMES, MAX_ANIMATION_FRAMES, PULSE_FRAMES, RIPPLE_FRAMES, SLIDE_FRAMES, breath_color,
    breath_intensity, flash_style, motion_disabled, pulse_style, ripple_active_at, slide_style,
};

#[test]
fn no_primitive_exceeds_500ms_at_2hz() {
    // The engagement-UX research called out >500ms as the boundary
    // where stale animations look broken. At 500ms refresh × the constant
    // we shouldn't blow past the documented MAX_ANIMATION_FRAMES.
    for f in [FLASH_FRAMES, PULSE_FRAMES, SLIDE_FRAMES, RIPPLE_FRAMES] {
        assert!(
            f <= MAX_ANIMATION_FRAMES,
            "primitive lifetime {f} exceeds {MAX_ANIMATION_FRAMES}"
        );
    }
}

#[test]
fn flash_style_only_modifies_on_active_frame() {
    let base = Style::default().fg(Color::Cyan);
    let off = flash_style(base, 0);
    let on = flash_style(base, 1);
    assert!(!off.add_modifier.contains(Modifier::REVERSED));
    assert!(on.add_modifier.contains(Modifier::REVERSED));
}

#[test]
fn pulse_style_walks_bold_to_dim_to_quiet() {
    let base = Style::default();
    // Frame 3 → BOLD (rising), 1 → DIM (falling), 0 → quiet.
    assert!(pulse_style(base, 3).add_modifier.contains(Modifier::BOLD));
    assert!(pulse_style(base, 1).add_modifier.contains(Modifier::DIM));
    let quiet = pulse_style(base, 0);
    assert!(!quiet.add_modifier.contains(Modifier::BOLD));
    assert!(!quiet.add_modifier.contains(Modifier::DIM));
}

#[test]
fn slide_style_inserts_then_emphasizes_then_settles() {
    let base = Style::default();
    assert!(
        slide_style(base, 2)
            .add_modifier
            .contains(Modifier::REVERSED)
    );
    assert!(slide_style(base, 1).add_modifier.contains(Modifier::BOLD));
    let settled = slide_style(base, 0);
    assert!(!settled.add_modifier.contains(Modifier::REVERSED));
    assert!(!settled.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn ripple_advances_one_row_per_frame() {
    // Cascade origin (distance 0) lights up on the first frame; row 1
    // on the second; row 2 on the third; etc.
    assert!(ripple_active_at(0, RIPPLE_FRAMES));
    assert!(ripple_active_at(1, RIPPLE_FRAMES - 1));
    assert!(ripple_active_at(2, RIPPLE_FRAMES - 2));
    // Outside the ripple's lit cell the function returns false.
    assert!(!ripple_active_at(0, RIPPLE_FRAMES - 1));
    assert!(!ripple_active_at(5, RIPPLE_FRAMES));
}

#[test]
fn breath_intensity_oscillates_smoothly() {
    let lows = (0..2).map(breath_intensity).collect::<Vec<_>>();
    let highs = (5..7).map(breath_intensity).collect::<Vec<_>>();
    // Trough samples should be close to zero; crest samples close to one.
    assert!(
        lows.iter().all(|v| *v < 0.30),
        "low-phase too bright: {lows:?}"
    );
    assert!(
        highs.iter().all(|v| *v > 0.70),
        "crest-phase too dim: {highs:?}"
    );
}

#[test]
fn breath_intensity_repeats_each_period() {
    // 12-tick period: tick 0 == tick 12 == tick 24.
    let a = breath_intensity(0);
    let b = breath_intensity(12);
    let c = breath_intensity(24);
    assert!((a - b).abs() < 1e-9);
    assert!((a - c).abs() < 1e-9);
}

#[test]
fn breath_color_walks_dark_to_green() {
    assert_eq!(breath_color(0.0), Color::DarkGray);
    assert_eq!(breath_color(0.5), Color::LightGreen);
    assert_eq!(breath_color(1.0), Color::Green);
}

#[test]
fn motion_disabled_respects_truthy_env() {
    unsafe { std::env::set_var("YGG_TUI_NO_MOTION", "1") };
    assert!(motion_disabled());
    unsafe { std::env::set_var("YGG_TUI_NO_MOTION", "off") };
    assert!(!motion_disabled());
    unsafe { std::env::remove_var("YGG_TUI_NO_MOTION") };
    assert!(!motion_disabled());
}
