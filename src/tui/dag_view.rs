use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use sqlx::PgPool;

use crate::models::agent::AgentRepo;
use crate::models::node::{Node, NodeKind, NodeRepo};

/// DAG tree viewer — renders the bead tree for a selected agent.
pub struct DagView {
    agent_name: Option<String>,
    nodes: Vec<Node>,
    scroll_offset: usize,
}

impl DagView {
    pub fn new() -> Self {
        Self {
            agent_name: None,
            nodes: vec![],
            scroll_offset: 0,
        }
    }

    pub fn set_agent(&mut self, name: String) {
        self.agent_name = Some(name);
        self.nodes.clear();
        self.scroll_offset = 0;
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        if self.scroll_offset < self.nodes.len().saturating_sub(1) {
            self.scroll_offset += 1;
        }
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let Some(ref name) = self.agent_name else {
            return Ok(());
        };

        let agent_repo = AgentRepo::new(pool);
        let agent = match agent_repo.get_by_name(name).await? {
            Some(a) => a,
            None => return Ok(()),
        };

        if let Some(head_id) = agent.head_node_id {
            let node_repo = NodeRepo::new(pool);
            self.nodes = node_repo.get_ancestor_path(head_id).await?;
        } else {
            self.nodes.clear();
        }

        Ok(())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let title = match &self.agent_name {
            Some(name) => format!(" DAG: {} ({} nodes) ", name, self.nodes.len()),
            None => " DAG: select an agent from dashboard (enter) ".to_string(),
        };

        if self.nodes.is_empty() {
            let msg = Paragraph::new("No nodes to display. Select an agent from the dashboard and press Enter.")
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(msg, area);
            return;
        }

        let items: Vec<ListItem> = self
            .nodes
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .map(|(i, node)| {
                let depth = node.ancestors.len();
                let indent = "  ".repeat(depth.min(10));
                let connector = if i == 0 { "○" } else { "├─○" };

                let kind_str = match node.kind {
                    NodeKind::UserMessage => "[user]",
                    NodeKind::AssistantMessage => "[asst]",
                    NodeKind::ToolCall => "[tool]",
                    NodeKind::ToolResult => "[result]",
                    NodeKind::Digest => "[digest]",
                    NodeKind::Directive => "[directive]",
                    NodeKind::System => "[sys]",
                    NodeKind::HumanOverride => "[human]",
                };

                let kind_color = match node.kind {
                    NodeKind::UserMessage => Color::Blue,
                    NodeKind::AssistantMessage => Color::Green,
                    NodeKind::ToolCall => Color::Yellow,
                    NodeKind::ToolResult => Color::Gray,
                    NodeKind::Digest => Color::Magenta,
                    NodeKind::HumanOverride => Color::Red,
                    _ => Color::White,
                };

                // Truncate content preview
                let content_preview = node
                    .content
                    .as_str()
                    .map(|s| s.chars().take(60).collect::<String>())
                    .or_else(|| {
                        node.content.get("task").and_then(|v| v.as_str()).map(|s| {
                            s.chars().take(60).collect()
                        })
                    })
                    .or_else(|| {
                        node.content.get("command").and_then(|v| v.as_str()).map(|s| {
                            s.chars().take(60).collect()
                        })
                    })
                    .unwrap_or_else(|| {
                        let s = node.content.to_string();
                        s.chars().take(60).collect()
                    });

                let line = Line::from(vec![
                    Span::raw(format!("{indent}{connector} ")),
                    Span::styled(kind_str, Style::default().fg(kind_color)),
                    Span::raw(format!(" {content_preview}")),
                    Span::styled(
                        format!("  ({}tok)", node.token_count),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);

                ListItem::new(line)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title));

        frame.render_widget(list, area);
    }
}
