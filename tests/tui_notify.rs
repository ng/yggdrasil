//! Regression for desktop-notify substrate (yggdrasil-135).
//! Spawns the OS-level notify command and shouldn't be exercised in
//! environments without `osascript` / `notify-send`. Most assertions
//! here are pure-Rust (no spawn) so the suite remains hermetic.

use ygg::notify::{NotifySeverity, notifications_disabled, notify};

#[test]
fn off_env_var_suppresses_notifications() {
    unsafe { std::env::set_var("YGG_NOTIFY", "off") };
    assert!(notifications_disabled());
    let res = notify("test", "body", NotifySeverity::Info).unwrap();
    assert!(!res, "off should short-circuit before spawn");
    unsafe { std::env::remove_var("YGG_NOTIFY") };
}

#[test]
fn other_truthy_disable_values_are_recognised() {
    for val in ["0", "false", "no"] {
        unsafe { std::env::set_var("YGG_NOTIFY", val) };
        assert!(notifications_disabled(), "{val} should disable");
    }
    unsafe { std::env::remove_var("YGG_NOTIFY") };
}

#[test]
fn unknown_value_does_not_disable() {
    unsafe { std::env::set_var("YGG_NOTIFY", "yes") };
    assert!(!notifications_disabled());
    unsafe { std::env::remove_var("YGG_NOTIFY") };
}

#[test]
fn severity_subtitles_distinguish_bands() {
    assert_eq!(NotifySeverity::Info.as_subtitle(), "Yggdrasil");
    assert_eq!(NotifySeverity::Warn.as_subtitle(), "Yggdrasil · attention");
    assert_eq!(
        NotifySeverity::Critical.as_subtitle(),
        "Yggdrasil · critical"
    );
}
