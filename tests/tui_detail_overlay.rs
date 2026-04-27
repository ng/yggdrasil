//! Regression for the floating detail overlay (yggdrasil-151).

use ygg::tui::app::App;

#[test]
fn open_detail_sets_overlay_state() {
    let mut app = App::new("test".into());
    assert!(app.detail_overlay.is_none());
    app.open_detail("title", "body");
    assert!(app.detail_overlay.is_some());
    let o = app.detail_overlay.as_ref().unwrap();
    assert_eq!(o.title, "title");
    assert_eq!(o.body, "body");
}

#[test]
fn close_detail_clears_overlay() {
    let mut app = App::new("test".into());
    app.open_detail("t", "b");
    app.close_detail();
    assert!(app.detail_overlay.is_none());
}

#[test]
fn close_detail_is_noop_when_already_closed() {
    let mut app = App::new("test".into());
    app.close_detail();
    assert!(app.detail_overlay.is_none());
}
