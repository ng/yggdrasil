//! Regression for the global repo/all scope toggle (yggdrasil-134).

use ygg::tui::app::Scope;

#[test]
fn scope_defaults_to_repo() {
    assert_eq!(Scope::default(), Scope::Repo);
}

#[test]
fn toggle_walks_repo_to_all_to_repo() {
    let mut s = Scope::default();
    assert_eq!(s, Scope::Repo);
    s.toggle();
    assert_eq!(s, Scope::All);
    s.toggle();
    assert_eq!(s, Scope::Repo);
}

#[test]
fn label_is_short_and_lowercase() {
    assert_eq!(Scope::Repo.label(), "repo");
    assert_eq!(Scope::All.label(), "all");
}
