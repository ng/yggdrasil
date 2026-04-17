//! Tasks pane — ready tasks across every registered repo (flat list).
//! Cwd-scoping silently shows nothing when the TUI runs from the wrong
//! directory; dashboard semantics are cross-system everywhere.

use std::collections::{HashMap, HashSet};

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::repo::RepoRepo;
use crate::models::task::{Task, TaskRepo, TaskStatus};

pub struct TasksView {
    pub rows: Vec<(String, Task)>, // (prefix, task)
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
        ready.sort_by_key(|(_p, t)| (t.priority, t.seq));
        self.rows = ready;

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
        if self.rows.is_empty() { return; }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(if i == 0 { self.rows.len() - 1 } else { i - 1 }));
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() { return; }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1) % self.rows.len()));
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
            Cell::from("KIND").style(Style::default().fg(Color::Gray)),
            Cell::from("TITLE").style(Style::default().fg(Color::Gray)),
        ]);

        let rows: Vec<Row> = self.rows.iter().map(|(prefix, t)| {
            let id = format!("{}-{}", prefix, t.seq);
            Row::new(vec![
                Cell::from(id),
                Cell::from(format!("P{}", t.priority))
                    .style(prio_style(t.priority)),
                Cell::from(t.kind.to_string()).style(Style::default().fg(Color::DarkGray)),
                Cell::from(t.title.clone()),
            ])
        }).collect();

        let table = Table::new(rows, [
            Constraint::Length(16),
            Constraint::Length(3),
            Constraint::Length(10),
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
