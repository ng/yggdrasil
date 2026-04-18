//! Shared TUI widgets. Keeps DAG, Tasks, Dashboard-Workers panes speaking
//! the same visual language — one bold "this is running" style, one
//! glyph map, one priority color scheme.

use ratatui::prelude::*;
use ratatui::widgets::Cell;

use crate::models::task::{Task, TaskKind, TaskStatus};

/// (glyph, color) for a task's run-state. Matches the DAG pane glyph set
/// callers had before the extraction.
pub fn run_state_glyph(t: &Task) -> (&'static str, Color, bool /* bold */) {
    match t.status {
        TaskStatus::Open       => ("◯", Color::DarkGray, false),
        TaskStatus::InProgress => ("▶", Color::Green,    true),
        TaskStatus::Blocked    => ("⏸", Color::Red,      false),
        TaskStatus::Closed     => {
            let failed = t.close_reason.as_deref()
                .map(|r| r.to_lowercase().contains("fail"))
                .unwrap_or(false);
            if failed { ("✗", Color::Red, false) }
            else { ("✓", Color::DarkGray, false) }
        }
    }
}

/// (glyph, color) for a task's kind.
pub fn kind_glyph(k: &TaskKind) -> (&'static str, Color) {
    match k {
        TaskKind::Epic    => ("◉", Color::Magenta),
        TaskKind::Feature => ("✚", Color::Cyan),
        TaskKind::Bug     => ("🐞", Color::Red),
        TaskKind::Chore   => ("·", Color::DarkGray),
        TaskKind::Task    => ("○", Color::White),
    }
}

pub fn priority_style(p: i16) -> Style {
    match p {
        0 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        1 => Style::default().fg(Color::Yellow),
        2 => Style::default().fg(Color::White),
        _ => Style::default().fg(Color::DarkGray),
    }
}

/// Title style: green+bold for in-progress so running tasks POP.
pub fn title_style(status: &TaskStatus) -> Style {
    if matches!(status, TaskStatus::InProgress) {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

/// Row cells for a task in table form (Tasks pane + future Workers pane).
/// Returns the cells in order: [RUN, KIND, ID, P, TITLE, AGE].
pub fn task_row_cells<'a>(t: &'a Task, prefix: &'a str) -> Vec<Cell<'a>> {
    let (run, run_color, run_bold) = run_state_glyph(t);
    let (kg, kc) = kind_glyph(&t.kind);
    let run_style = if run_bold {
        Style::default().fg(run_color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(run_color)
    };
    let age_secs = (chrono::Utc::now() - t.updated_at).num_seconds().max(0);
    let age_style = if age_secs >= 7 * 86400 { Style::default().fg(Color::Yellow) }
                    else if age_secs >= 86400 { Style::default().fg(Color::DarkGray) }
                    else { Style::default().fg(Color::Gray) };
    vec![
        Cell::from(run).style(run_style),
        Cell::from(format!("{kg} {:?}", t.kind).to_lowercase()).style(Style::default().fg(kc)),
        Cell::from(format!("{prefix}-{}", t.seq)).style(Style::default().fg(Color::Gray)),
        Cell::from(format!("P{}", t.priority)).style(priority_style(t.priority)),
        Cell::from(t.title.clone()).style(title_style(&t.status)),
        Cell::from(humanize_age(age_secs)).style(age_style),
    ]
}

fn humanize_age(secs: i64) -> String {
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else if secs < 86400 { format!("{}h", secs / 3600) }
    else if secs < 7 * 86400 { format!("{}d", secs / 86400) }
    else { format!("{}w", secs / (7 * 86400)) }
}
