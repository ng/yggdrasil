//! Tasks pane — `ygg task ready` but live.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};
use sqlx::PgPool;

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::models::task::{Task, TaskRepo};

pub struct TasksView {
    pub tasks: Vec<Task>,
    pub prefix: String,
    pub state: TableState,
}

impl TasksView {
    pub fn new() -> Self {
        let mut st = TableState::default();
        st.select(Some(0));
        Self { tasks: vec![], prefix: String::new(), state: st }
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        match resolve_cwd_repo(pool).await {
            Ok(r) => {
                self.prefix = r.task_prefix.clone();
                self.tasks = TaskRepo::new(pool).ready(r.repo_id).await.unwrap_or_default();
            }
            Err(_) => {
                self.tasks.clear();
                self.prefix = "?".into();
            }
        }
        if self.tasks.is_empty() {
            self.state.select(None);
        } else if self.state.selected().unwrap_or(0) >= self.tasks.len() {
            self.state.select(Some(self.tasks.len() - 1));
        }
        Ok(())
    }

    pub fn select_prev(&mut self) {
        if self.tasks.is_empty() { return; }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(if i == 0 { self.tasks.len() - 1 } else { i - 1 }));
    }

    pub fn select_next(&mut self) {
        if self.tasks.is_empty() { return; }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1) % self.tasks.len()));
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("ID").style(Style::default().fg(Color::Gray)),
            Cell::from("P").style(Style::default().fg(Color::Gray)),
            Cell::from("KIND").style(Style::default().fg(Color::Gray)),
            Cell::from("TITLE").style(Style::default().fg(Color::Gray)),
        ]);

        let rows: Vec<Row> = self.tasks.iter().map(|t| {
            let id = format!("{}-{}", self.prefix, t.seq);
            Row::new(vec![
                Cell::from(id),
                Cell::from(format!("P{}", t.priority))
                    .style(prio_style(t.priority)),
                Cell::from(t.kind.to_string()).style(Style::default().fg(Color::DarkGray)),
                Cell::from(t.title.clone()),
            ])
        }).collect();

        let title = format!(" Tasks — {} ready in {}  ({} total) ",
            self.tasks.len(), self.prefix, self.tasks.len());

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
