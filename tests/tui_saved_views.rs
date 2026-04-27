//! Regression for the saved-views file format + upsert/remove (yggdrasil-131).

use std::collections::BTreeMap;
use ygg::tui::saved_views::{SavedView, SavedViewsFile, load_from, save_to};

fn fixture_view(name: &str) -> SavedView {
    SavedView {
        name: name.into(),
        pane: "Tasks".into(),
        scope: "repo".into(),
        filter: BTreeMap::new(),
    }
}

#[test]
fn missing_file_loads_empty_set_not_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does-not-exist.toml");
    let file = load_from(&path).unwrap();
    assert!(file.views.is_empty());
}

#[test]
fn save_then_load_round_trips_names_and_panes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("views.toml");

    let mut file = SavedViewsFile::default();
    file.upsert(fixture_view("morning-triage"));
    file.upsert(fixture_view("hot-tasks"));
    save_to(&path, &file).unwrap();

    let back = load_from(&path).unwrap();
    assert_eq!(back.views.len(), 2);
    assert_eq!(
        back.get("morning-triage").map(|v| v.pane.as_str()),
        Some("Tasks")
    );
}

#[test]
fn upsert_replaces_existing_view_by_name() {
    let mut file = SavedViewsFile::default();
    file.upsert(fixture_view("v1"));
    let mut updated = fixture_view("v1");
    updated.scope = "all".into();
    file.upsert(updated);
    assert_eq!(file.views.len(), 1);
    assert_eq!(file.get("v1").unwrap().scope, "all");
}

#[test]
fn remove_drops_only_named_view() {
    let mut file = SavedViewsFile::default();
    file.upsert(fixture_view("a"));
    file.upsert(fixture_view("b"));
    file.remove("a");
    assert_eq!(file.views.len(), 1);
    assert_eq!(file.views[0].name, "b");
}

#[test]
fn remove_missing_view_is_noop() {
    let mut file = SavedViewsFile::default();
    file.upsert(fixture_view("only"));
    file.remove("not-there");
    assert_eq!(file.views.len(), 1);
}

#[test]
fn parents_are_created_on_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/parent/views.toml");
    save_to(&path, &SavedViewsFile::default()).unwrap();
    assert!(path.exists());
}
