//! Knowledge pane: grouping, scope filtering, selection and rendering
//! against a synthetic browse report (no corpus on disk, no Postgres).

use std::collections::BTreeMap;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use uuid::Uuid;
use ygg::knowledge::store::{BrowseDocument, BrowseReport};
use ygg::tui::app::Scope;
use ygg::tui::knowledge_view::{KnowledgeRow, KnowledgeView};

fn doc(path: &str, document_type: &str) -> BrowseDocument {
    BrowseDocument {
        path: path.into(),
        revision: "0123456789abcdef".into(),
        document_type: document_type.into(),
        text: None,
    }
}

fn report(repo_a: Uuid, repo_b: Uuid) -> BrowseReport {
    BrowseReport {
        documents: vec![
            doc(
                &format!("repos/{repo_b}/learnings/11111111-0000-0000-0000-000000000000.md"),
                "Engineering Rule",
            ),
            doc(
                &format!("repos/{repo_a}/notes/22222222-0000-0000-0000-000000000000.md"),
                "Note",
            ),
            doc(
                "global/notes/33333333-0000-0000-0000-000000000000.md",
                "Note",
            ),
            doc("misc/readme.md", "Generic"),
        ],
        diagnostics: vec![],
        shared_commit: Some("abcdef0123456789".into()),
        shared_current: Some(true),
    }
}

fn labels(repo_a: Uuid, repo_b: Uuid) -> BTreeMap<Uuid, String> {
    BTreeMap::from([
        (repo_a, "ng/yggdrasil".to_string()),
        (repo_b, "ng/other".to_string()),
    ])
}

fn render(view: &mut KnowledgeView, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn groups_global_first_then_current_repo_then_others() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    view.apply(report(a, b), labels(a, b), Some(a), Scope::All);

    let headers: Vec<String> = view
        .rows
        .iter()
        .filter_map(|r| match r {
            KnowledgeRow::Header { label, count } => Some(format!("{label}:{count}")),
            KnowledgeRow::Doc(_) => None,
        })
        .collect();
    assert_eq!(
        headers,
        vec!["global:1", "ng/yggdrasil:1", "ng/other:1", "other:1"]
    );
    assert_eq!(view.doc_count(), 4);
    // Selection never rests on a header.
    assert!(view.selected_doc().is_some());
    assert!(view.selected_doc().unwrap().path.starts_with("global/"));
}

#[test]
fn repo_scope_keeps_global_and_current_repo_only() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    view.apply(report(a, b), labels(a, b), Some(a), Scope::Repo);
    assert_eq!(view.doc_count(), 2);
    let paths: Vec<&str> = view
        .rows
        .iter()
        .filter_map(|r| match r {
            KnowledgeRow::Doc(d) => Some(d.path.as_str()),
            _ => None,
        })
        .collect();
    assert!(paths[0].starts_with("global/"));
    assert!(paths[1].contains(&a.to_string()));
}

#[test]
fn repo_scope_without_resolved_repo_shows_everything() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    view.apply(report(a, b), labels(a, b), None, Scope::Repo);
    assert_eq!(view.doc_count(), 4);
}

#[test]
fn selection_skips_headers_and_clamps_at_edges() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    view.apply(report(a, b), labels(a, b), Some(a), Scope::All);
    let first = view.selected;
    view.select_prev();
    assert_eq!(view.selected, first);
    for _ in 0..10 {
        view.select_next();
        assert!(view.selected_doc().is_some());
    }
    let last = view.selected;
    view.select_next();
    assert_eq!(view.selected, last);
    assert_eq!(view.selected_doc().unwrap().path, "misc/readme.md");
}

#[test]
fn selection_stays_visible_past_the_viewport() {
    let docs = (0..30)
        .map(|i| {
            doc(
                &format!("global/notes/{i:08}-0000-0000-0000-000000000000.md"),
                "Note",
            )
        })
        .collect();
    let mut view = KnowledgeView::new();
    view.apply(
        BrowseReport {
            documents: docs,
            ..BrowseReport::default()
        },
        BTreeMap::new(),
        None,
        Scope::All,
    );
    for _ in 0..25 {
        view.select_next();
    }
    let stem = view.selected_doc().unwrap().path[13..21].to_string();
    let screen = render(&mut view, 100, 10);
    assert!(
        screen.contains(&stem),
        "selected {stem} scrolled off:\n{screen}"
    );
}

#[test]
fn repos_sharing_a_label_stay_in_separate_groups() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    let same = BTreeMap::from([(a, "ng/app".to_string()), (b, "ng/app".to_string())]);
    view.apply(report(a, b), same, None, Scope::All);
    let headers: Vec<(String, usize)> = view
        .rows
        .iter()
        .filter_map(|r| match r {
            KnowledgeRow::Header { label, count } => Some((label.clone(), *count)),
            KnowledgeRow::Doc(_) => None,
        })
        .collect();
    let app_groups: Vec<_> = headers
        .iter()
        .filter(|(l, _)| l.starts_with("ng/app"))
        .collect();
    assert_eq!(app_groups.len(), 2, "{headers:?}");
    assert!(
        app_groups.iter().all(|(l, n)| l.contains('…') && *n == 1),
        "{headers:?}"
    );
}

#[test]
fn narrowing_scope_closes_an_out_of_scope_body() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    view.apply(report(a, b), labels(a, b), Some(a), Scope::All);
    let other = format!("repos/{b}/learnings/11111111-0000-0000-0000-000000000000.md");
    view.set_body(other, "rule text");
    view.apply(report(a, b), labels(a, b), Some(a), Scope::Repo);
    assert!(!view.body_open());
}

#[test]
fn page_down_stops_at_the_end_of_the_document() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    view.apply(report(a, b), labels(a, b), Some(a), Scope::All);
    view.set_body("global/notes/x.md", "line one\nline two\nlast line");
    view.scroll_body_down(50);
    let screen = render(&mut view, 120, 20);
    assert_eq!(view.body_scroll, 0, "short body must not scroll");
    assert!(screen.contains("last line"), "{screen}");

    let long: String = (0..40).map(|i| format!("row {i}\n")).collect();
    view.set_body("global/notes/x.md", long);
    view.scroll_body_down(500);
    let screen = render(&mut view, 120, 20);
    assert!(
        screen.contains("row 39"),
        "end must stay visible:\n{screen}"
    );
    assert!(view.body_scroll > 0 && view.body_scroll < 40);
}

#[test]
fn unresolved_repo_scope_says_it_shows_everything() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    view.apply(report(a, b), labels(a, b), None, Scope::Repo);
    let screen = render(&mut view, 160, 20);
    assert!(screen.contains("repo unresolved, showing all"), "{screen}");
}

#[test]
fn renders_groups_kinds_and_open_body() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut view = KnowledgeView::new();
    view.apply(report(a, b), labels(a, b), Some(a), Scope::All);

    let screen = render(&mut view, 120, 20);
    assert!(screen.contains("Knowledge (4 docs)"), "{screen}");
    assert!(screen.contains("shared abcdef01 (current)"), "{screen}");
    assert!(screen.contains("▾ global (1)"), "{screen}");
    assert!(screen.contains("▾ ng/yggdrasil (1)"), "{screen}");
    assert!(screen.contains("note"), "{screen}");
    assert!(screen.contains("rule"), "{screen}");
    assert!(screen.contains("Engineering Rule"), "{screen}");

    view.set_body(
        "global/notes/x.md",
        "---\ntype: Note\n---\nalways run make install",
    );
    let screen = render(&mut view, 120, 20);
    assert!(screen.contains("global/notes/x.md"), "{screen}");
    assert!(screen.contains("always run make install"), "{screen}");
    assert!(view.body_open());
    view.close_body();
    assert!(!view.body_open());
}

#[test]
fn empty_state_explains_itself() {
    let mut view = KnowledgeView::new();
    let screen = render(&mut view, 80, 6);
    assert!(screen.contains("loading"), "{screen}");

    view.apply(BrowseReport::default(), BTreeMap::new(), None, Scope::Repo);
    let screen = render(&mut view, 80, 6);
    assert!(screen.contains("no documents in scope"), "{screen}");
}

#[test]
fn diagnostics_surface_in_status() {
    let mut view = KnowledgeView::new();
    view.apply(
        BrowseReport {
            documents: vec![doc("global/notes/a.md", "Note")],
            diagnostics: vec!["\"repos/bad\": browse requires a Markdown document".into()],
            shared_commit: None,
            shared_current: None,
        },
        BTreeMap::new(),
        None,
        Scope::All,
    );
    assert_eq!(view.status, "1 diagnostics (listing incomplete)");
    let screen = render(&mut view, 120, 12);
    assert!(screen.contains("1 diagnostics"), "{screen}");
    assert!(
        screen.contains("browse requires a Markdown document"),
        "diagnostic detail should be listed:\n{screen}"
    );
}

/// Walks the operator's real selected corpus. Needs `ygg init` to have run
/// on this machine, so it is opt-in: `cargo test --test tui_knowledge_view -- --ignored`.
#[test]
#[ignore]
fn smoke_real_corpus_refresh() {
    let mut view = KnowledgeView::new();
    view.refresh(Scope::All, true);
    wait_idle(&mut view);
    assert!(view.loaded);
    assert!(
        view.status.is_empty(),
        "refresh reported a problem: {}",
        view.status
    );
    assert!(view.doc_count() > 0, "no documents in the selected corpus");
    view.toggle_body();
    wait_idle(&mut view);
    assert!(view.body_open(), "Enter should load the selected document");
    let screen = render(&mut view, 140, 30);
    assert!(screen.contains("Knowledge ("), "{screen}");
    println!("{screen}");
}

fn wait_idle(view: &mut KnowledgeView) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while view.is_loading() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
        view.poll();
    }
}
