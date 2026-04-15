use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use sqlx::PgPool;

use crate::lock::LockManager;
use crate::models::agent::{AgentRepo, AgentWorkflow};

/// Dashboard view showing all agents, locks, and summary stats.
pub struct DashboardView {
    agents: Vec<AgentWorkflow>,
    locks: Vec<crate::lock::ResourceLock>,
    selected: usize,
}

impl DashboardView {
    pub fn new() -> Self {
        Self {
            agents: vec![],
            locks: vec![],
            selected: 0,
        }
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let agent_repo = AgentRepo::new(pool);
        self.agents = agent_repo.list().await?;

        let lock_mgr = LockManager::new(pool, 300);
        self.locks = lock_mgr.list_all().await?;

        Ok(())
    }

    pub fn select_next(&mut self) {
        if !self.agents.is_empty() {
            self.selected = (self.selected + 1).min(self.agents.len() - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected_agent(&self) -> Option<String> {
        self.agents.get(self.selected).map(|a| a.agent_name.clone())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Length(8),
                Constraint::Length(3),
            ])
            .split(area);

        self.render_agents_table(frame, chunks[0]);
        self.render_locks_table(frame, chunks[1]);
        self.render_summary(frame, chunks[2]);
    }

    fn render_agents_table(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("NAME").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Cell::from("STATE"),
            Cell::from("PRESSURE"),
            Cell::from("HEAD"),
            Cell::from("UPDATED"),
        ])
        .height(1);

        let rows: Vec<Row> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let style = if i == self.selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                let state_color = match agent.current_state {
                    crate::models::agent::AgentState::Executing => Color::Green,
                    crate::models::agent::AgentState::Idle => Color::Gray,
                    crate::models::agent::AgentState::HumanOverride => Color::Yellow,
                    crate::models::agent::AgentState::Error => Color::Red,
                    _ => Color::White,
                };

                let pressure_pct = if agent.context_tokens > 0 {
                    (agent.context_tokens as f64 / 250000.0 * 100.0) as u32
                } else {
                    0
                };

                let pressure_bar = format!(
                    "{} {}%",
                    "█".repeat((pressure_pct / 10) as usize).chars().take(10).collect::<String>(),
                    pressure_pct,
                );

                Row::new(vec![
                    Cell::from(agent.agent_name.clone()),
                    Cell::from(agent.current_state.to_string()).style(Style::default().fg(state_color)),
                    Cell::from(pressure_bar),
                    Cell::from(
                        agent.head_node_id.map(|id| id.to_string()[..8].to_string()).unwrap_or_else(|| "—".into()),
                    ),
                    Cell::from(agent.updated_at.format("%H:%M:%S").to_string()),
                ])
                .style(style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(15),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Agents "));

        frame.render_widget(table, area);
    }

    fn render_locks_table(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("RESOURCE").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Cell::from("AGENT"),
            Cell::from("EXPIRES"),
        ]);

        let rows: Vec<Row> = self
            .locks
            .iter()
            .map(|lock| {
                Row::new(vec![
                    Cell::from(lock.resource_key.clone()),
                    Cell::from(lock.agent_id.to_string()[..8].to_string()),
                    Cell::from(lock.expires_at.format("%H:%M:%S").to_string()),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(40),
                Constraint::Percentage(30),
                Constraint::Percentage(30),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Locks "));

        frame.render_widget(table, area);
    }

    fn render_summary(&self, frame: &mut Frame, area: Rect) {
        let active = self.agents.iter().filter(|a| {
            a.current_state == crate::models::agent::AgentState::Executing
        }).count();

        let text = format!(
            " {} agent(s) | {} active | {} lock(s)",
            self.agents.len(),
            active,
            self.locks.len(),
        );

        let para = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(" Summary "));

        frame.render_widget(para, area);
    }
}
