//! Regression for the narrow-terminal collapse (yggdrasil-156).
//! Verifies the threshold and that the collapsed chrome actually fits in
//! an 80-column terminal — the worst realistic narrow case (notebooks,
//! sidebar panes, half-screen tmux splits).

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ygg::tui::app::{App, NARROW_TERMINAL_THRESHOLD};

#[test]
fn narrow_threshold_is_100_columns() {
    // Documenting the magic number so changes here force a test failure
    // and a reviewer eyeballs the implications.
    assert_eq!(NARROW_TERMINAL_THRESHOLD, 100);
}

fn render_at(width: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
    let mut app = App::new("test-agent".to_string());
    term.draw(|f| app.draw_for_test(f)).unwrap();
    term.backend().buffer().clone()
}

fn first_row(buf: &Buffer) -> String {
    let width = buf.area().width;
    (0..width)
        .map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' '))
        .collect()
}

#[test]
fn wide_terminal_renders_full_tab_labels() {
    let buf = render_at(160);
    let row = first_row(&buf);
    assert!(
        row.contains("Dashboard") && row.contains("DAG") && row.contains("Tasks"),
        "wide row should carry full labels: {row:?}"
    );
}

#[test]
fn narrow_terminal_renders_compact_tab_labels_only() {
    let buf = render_at(80);
    let row = first_row(&buf);
    assert!(
        !row.contains("Dashboard"),
        "narrow row should NOT carry the word 'Dashboard': {row:?}"
    );
    // Every tab activator remains available in compact chrome.
    for key in ["1", "2", "3", "5", "9", "0", "G", "N", "C", "K"] {
        assert!(
            row.contains(key),
            "narrow row missing activator {key}: {row:?}"
        );
    }
}

#[test]
fn tab_row_compacts_when_full_labels_would_clip() {
    // Above the 100-column threshold but narrower than the full tab row:
    // compact activators beat a row whose last tabs fall off the edge.
    let buf = render_at(105);
    let row = first_row(&buf);
    assert!(!row.contains("Dashboard"), "{row:?}");
    assert!(row.contains(" K "), "Knowledge activator clipped: {row:?}");
}

#[test]
fn narrow_terminal_drops_orchestration_panel() {
    let buf = render_at(80);
    let mut all = String::new();
    for y in 0..buf.area().height {
        for x in 0..buf.area().width {
            all.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        all.push('\n');
    }
    // The orchestration panel's title is "orchestration"; it must not
    // render when we collapsed the strip to a single column.
    assert!(
        !all.contains("orchestration"),
        "narrow render must drop the ops panel: \n{all}"
    );
}

#[test]
fn wide_terminal_keeps_orchestration_panel() {
    let buf = render_at(160);
    let mut all = String::new();
    for y in 0..buf.area().height {
        for x in 0..buf.area().width {
            all.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        all.push('\n');
    }
    assert!(
        all.contains("orchestration"),
        "wide render must keep the ops panel: \n{all}"
    );
}

#[test]
fn boundary_width_99_collapses_width_100_does_not() {
    // The threshold governs the orchestration panel; the tab row picks
    // full labels only once they fit (they need more than 100 columns).
    let render_all = |width: u16| {
        let buf = render_at(width);
        let mut all = String::new();
        for y in 0..buf.area().height {
            for x in 0..buf.area().width {
                all.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            all.push('\n');
        }
        all
    };
    assert!(
        !render_all(99).contains("orchestration"),
        "width 99 must collapse"
    );
    assert!(
        render_all(100).contains("orchestration"),
        "width 100 must stay wide"
    );

    let narrow_row = first_row(&render_at(99));
    assert!(
        !narrow_row.contains("Dashboard"),
        "width 99 must collapse: {narrow_row:?}"
    );
    let full_row = first_row(&render_at(120));
    assert!(
        full_row.contains("Dashboard") && full_row.contains("Knowledge"),
        "full tab row must fit by 120 columns: {full_row:?}"
    );
}
