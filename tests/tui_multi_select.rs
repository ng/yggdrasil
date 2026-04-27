//! Regression for the generic multi-select state machine (yggdrasil-129).

use ygg::tui::multi_select::MultiSelect;

#[test]
fn fresh_selection_is_empty() {
    let s: MultiSelect<i32> = MultiSelect::new();
    assert!(s.is_empty());
    assert_eq!(s.len(), 0);
}

#[test]
fn toggle_round_trips() {
    let mut s: MultiSelect<i32> = MultiSelect::new();
    assert!(s.toggle(1));
    assert!(s.is_selected(&1));
    assert!(!s.toggle(1));
    assert!(!s.is_selected(&1));
}

#[test]
fn select_all_extends_rather_than_replaces() {
    let mut s: MultiSelect<i32> = MultiSelect::new();
    s.toggle(1);
    s.select_all([2, 3, 4]);
    assert_eq!(s.len(), 4);
    assert!(s.is_selected(&1));
    assert!(s.is_selected(&4));
}

#[test]
fn deselect_drops_only_named_keys() {
    let mut s: MultiSelect<i32> = MultiSelect::new();
    s.select_all([1, 2, 3]);
    s.deselect([2, 5]); // 5 was never selected — no-op
    assert_eq!(s.snapshot(), vec![1, 3]);
}

#[test]
fn clear_empties_selection() {
    let mut s: MultiSelect<i32> = MultiSelect::new();
    s.select_all([1, 2, 3]);
    s.clear();
    assert!(s.is_empty());
}

#[test]
fn snapshot_is_sorted_by_key_order() {
    let mut s: MultiSelect<i32> = MultiSelect::new();
    s.toggle(3);
    s.toggle(1);
    s.toggle(2);
    assert_eq!(s.snapshot(), vec![1, 2, 3]);
}

#[test]
fn works_with_string_keys_for_locks_pane() {
    let mut s: MultiSelect<String> = MultiSelect::new();
    s.toggle("src/db.rs".into());
    s.toggle("src/main.rs".into());
    assert_eq!(s.len(), 2);
    let keys = s.snapshot();
    assert!(keys.contains(&"src/db.rs".to_string()));
}
