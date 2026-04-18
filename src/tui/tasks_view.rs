//! Tasks pane — ready tasks across every registered repo (flat list).
//! Cwd-scoping silently shows nothing when the TUI runs from the wrong
//! directory; dashboard semantics are cross-system everywhere.

use std::collections::{HashMap, HashSet};

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::repo::RepoRepo;
use crate::models::task::{Task, TaskKind, TaskRepo, TaskStatus};

pub enum TaskRow {
    Header { kind: TaskKind, count: usize },
    Task { prefix: String, task: Task },
}

pub struct TasksView {
    pub rows: Vec<TaskRow>,
    pub state: TableState,
    pub last_status: String,
}

impl TasksView {
    pub fn new() -> Self {
        let mut st = TableState::default();
        st.select(Some(0));
        Self { rows: vec![], state: st, last_status: String::new() }
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        self.last_status.clear();
        let repos = RepoRepo::new(pool).list().await.unwrap_or_default();
        let prefix_by_repo: HashMap<Uuid, String> = repos.iter()
            .map(|r| (r.repo_id, r.task_prefix.clone())).collect();

        let tasks: Vec<Task> = TaskRepo::new(pool).list(None, None).await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.status != TaskStatus::Closed)
            .collect();

        let task_ids: Vec<Uuid> = tasks.iter().map(|t| t.task_id).collect();
        let edges: Vec<(Uuid, Uuid)> = sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT task_id, blocker_id FROM task_deps WHERE task_id = ANY($1)"
        ).bind(&task_ids).fetch_all(pool).await.unwrap_or_default();

        // A task is blocked iff one of its blockers is still open/in-progress.
        let open_ids: HashSet<Uuid> = tasks.iter()
            .filter(|t| matches!(t.status, TaskStatus::Open | TaskStatus::InProgress | TaskStatus::Blocked))
            .map(|t| t.task_id).collect();
        let mut blocked: HashSet<Uuid> = HashSet::new();
        for (tid, bid) in &edges {
            if open_ids.contains(bid) { blocked.insert(*tid); }
        }

        let mut ready: Vec<(String, Task)> = tasks.into_iter()
            .filter(|t| !blocked.contains(&t.task_id))
            .filter_map(|t| {
                let p = prefix_by_repo.get(&t.repo_id).cloned()?;
                Some((p, t))
            })
            .collect();
        // Group by kind (epic, feature, bug, task, chore); within each group
        // sort by priority then seq.
        ready.sort_by_key(|(_p, t)| (kind_order(&t.kind), t.priority, t.seq));

        // Walk the sorted list and emit a header row whenever the kind changes.
        let mut grouped: Vec<TaskRow> = Vec::new();
        let mut it = ready.into_iter().peekable();
        while let Some((prefix, task)) = it.next() {
            let kind = task.kind.clone();
            let mut bucket: Vec<(String, Task)> = vec![(prefix, task)];
            while let Some((_, peek)) = it.peek() {
                if peek.kind == kind {
                    if let Some(next) = it.next() { bucket.push(next); }
                } else { break; }
            }
            grouped.push(TaskRow::Header { kind, count: bucket.len() });
            for (p, t) in bucket {
                grouped.push(TaskRow::Task { prefix: p, task: t });
            }
        }
        self.rows = grouped;

        if self.rows.is_empty() {
            self.last_status = format!(
                "no ready tasks in any of {} registered repo(s)", repos.len()
            );
            self.state.select(None);
        } else if self.state.selected().unwrap_or(0) >= self.rows.len() {
            self.state.select(Some(self.rows.len() - 1));
        }
        Ok(())
    }

    pub fn select_prev(&mut self) {
        let Some(target) = self.neighbor_task_row(-1) else { return; };
        self.state.select(Some(target));
    }

    pub fn select_next(&mut self) {
        let Some(target) = self.neighbor_task_row(1) else { return; };
        self.state.select(Some(target));
    }

    /// Walk the row list in `dir` (±1), skipping headers. Wraps around.
    fn neighbor_task_row(&self, dir: isize) -> Option<usize> {
        if self.rows.is_empty() { return None; }
        let len = self.rows.len();
        let start = self.state.selected().unwrap_or(0);
        let mut i = start;
        for _ in 0..len {
            i = ((i as isize + dir).rem_euclid(len as isize)) as usize;
            if matches!(self.rows[i], TaskRow::Task { .. }) { return Some(i); }
        }
        None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!(" Tasks ready ({} across all repos) ", self.rows.len());

        if self.rows.is_empty() {
            let lines = vec![
                Line::from(""),
                Line::from("  No ready tasks in any registered repo."),
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Try "),
                    Span::styled("ygg task create \"...\" --kind task --priority 2",
                        Style::default().fg(Color::Cyan)),
                    Span::raw(" from inside a project."),
                ]),
                Line::from(""),
                if !self.last_status.is_empty() {
                    Line::from(vec![
                        Span::styled("  · ", Style::default().fg(Color::DarkGray)),
                        Span::styled(self.last_status.clone(), Style::default().fg(Color::DarkGray)),
                    ])
                } else { Line::from("") },
            ];
            let para = Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("ID").style(Style::default().fg(Color::Gray)),
            Cell::from("P").style(Style::default().fg(Color::Gray)),
            Cell::from("TITLE").style(Style::default().fg(Color::Gray)),
        ]);

        let rows: Vec<Row> = self.rows.iter().map(|row| match row {
            TaskRow::Header { kind, count } => {
                let (color, glyph) = kind_style(kind);
                Row::new(vec![
                    Cell::from(format!("{glyph} {}", pluralize_kind(kind)))
                        .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Cell::from(""),
                    Cell::from(format!("({count})"))
                        .style(Style::default().fg(Color::DarkGray)),
                ])
            }
            TaskRow::Task { prefix, task: t } => {
                Row::new(vec![
                    Cell::from(format!("  {}-{}", prefix, t.seq))
                        .style(Style::default().fg(Color::Gray)),
                    Cell::from(format!("P{}", t.priority)).style(prio_style(t.priority)),
                    Cell::from(t.title.clone()),
                ])
            }
        }).collect();

        let table = Table::new(rows, [
            Constraint::Length(20),
            Constraint::Length(3),
            Constraint::Min(20),
        ])
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

        frame.render_stateful_widget(table, area, &mut self.state);
    }
}

fn prio_style(p: i16) -> Style {
    match p {
        0 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        1 => Style::default().fg(Color::Yellow),
        2 => Style::default().fg(Color::White),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn kind_order(k: &TaskKind) -> u8 {
    match k {
        TaskKind::Epic => 0,
        TaskKind::Feature => 1,
        TaskKind::Bug => 2,
        TaskKind::Task => 3,
        TaskKind::Chore => 4,
    }
}

fn kind_style(k: &TaskKind) -> (Color, &'static str) {
    match k {
        TaskKind::Epic => (Color::Magenta, "◉"),
        TaskKind::Feature => (Color::Cyan, "✚"),
        TaskKind::Bug => (Color::Red, "✗"),
        TaskKind::Task => (Color::White, "○"),
        TaskKind::Chore => (Color::DarkGray, "·"),
    }
}

fn pluralize_kind(k: &TaskKind) -> &'static str {
    match k {
        TaskKind::Epic => "EPICS",
        TaskKind::Feature => "FEATURES",
        TaskKind::Bug => "BUGS",
        TaskKind::Task => "TASKS",
        TaskKind::Chore => "CHORES",
    }
}
