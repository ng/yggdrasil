//! Regression tests for the transient toast strip (yggdrasil-153).

use std::time::Duration;
use ygg::tui::app::{App, TOAST_DEFAULT_TTL, TOAST_MAX_VISIBLE, ToastSeverity};

#[test]
fn empty_app_reserves_no_toast_rows() {
    let app = App::new("test".into());
    assert_eq!(app.visible_toast_rows(), 0);
}

#[test]
fn pushed_toasts_reserve_rows_up_to_cap() {
    let mut app = App::new("test".into());
    for i in 0..5 {
        app.push_toast(format!("msg {i}"), ToastSeverity::Info, TOAST_DEFAULT_TTL);
    }
    // Five pushed; only TOAST_MAX_VISIBLE rows should be reserved.
    assert_eq!(app.visible_toast_rows() as usize, TOAST_MAX_VISIBLE);
}

#[test]
fn prune_drops_expired_toasts() {
    let mut app = App::new("test".into());
    app.push_toast("dies fast", ToastSeverity::Info, Duration::from_millis(1));
    app.push_toast("survives", ToastSeverity::Info, Duration::from_secs(60));
    std::thread::sleep(Duration::from_millis(50));
    app.prune_toasts();
    assert_eq!(app.toasts.len(), 1);
    assert_eq!(app.toasts[0].msg, "survives");
}

#[test]
fn prune_clamps_to_max_visible() {
    let mut app = App::new("test".into());
    for i in 0..(TOAST_MAX_VISIBLE * 2) {
        app.push_toast(
            format!("m{i}"),
            ToastSeverity::Info,
            Duration::from_secs(60),
        );
    }
    app.prune_toasts();
    assert_eq!(app.toasts.len(), TOAST_MAX_VISIBLE);
    // Oldest should have been dropped — last remaining message is the
    // most recently pushed.
    let last = app.toasts.last().unwrap();
    assert_eq!(last.msg, format!("m{}", TOAST_MAX_VISIBLE * 2 - 1));
}

#[test]
fn severity_round_trips_through_push() {
    let mut app = App::new("test".into());
    app.push_toast("warned", ToastSeverity::Warn, TOAST_DEFAULT_TTL);
    assert_eq!(app.toasts[0].severity, ToastSeverity::Warn);
}
