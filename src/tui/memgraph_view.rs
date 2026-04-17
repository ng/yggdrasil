//! Memgraph pane — memory similarity explorer.
//!
//! Top half: recent high-signal nodes (directive, digest, user_message) with
//! glyph + agent + age + snippet.
//! Bottom half: top-8 cosine neighbors of the focus node with similarity %.
//! Arrow keys move the cursor within the active section; Tab toggles the
//! cursor between top (list) and bottom (neighbors); Enter re-centers the
//! graph on the selected neighbor. The vectors we already store do the work
//! — no similarity edges are persisted, every centering triggers a fresh
//! k-NN query against the HNSW index.

use chrono::{DateTime, Utc};
use pgvector::Vector;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct MemNode {
    pub id: Uuid,
    pub kind: String,
    pub agent_name: String,
    pub created_at: DateTime<Utc>,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct Neighbor {
    pub id: Uuid,
    pub kind: String,
    pub agent_name: String,
    pub similarity: f64,
    pub snippet: String,
    pub created_at: DateTime<Utc>,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Focus { Recent, Neighbors }

pub struct MemGraphView {
    pub recent: Vec<MemNode>,
    pub recent_state: ListState,
    pub neighbors: Vec<Neighbor>,
    pub neighbor_state: ListState,
    pub focus_id: Option<Uuid>,
    pub focus_label: String,
    pub active: Focus,
    pub last_status: String,
}

impl MemGraphView {
    pub fn new() -> Self {
        let mut s = ListState::default(); s.select(Some(0));
        Self {
            recent: vec![], recent_state: s,
            neighbors: vec![], neighbor_state: ListState::default(),
            focus_id: None, focus_label: String::new(),
            active: Focus::Recent,
            last_status: String::new(),
        }
    }

    pub fn scroll_up(&mut self) {
        let (list_len, state) = match self.active {
            Focus::Recent => (self.recent.len(), &mut self.recent_state),
            Focus::Neighbors => (self.neighbors.len(), &mut self.neighbor_state),
        };
        if list_len == 0 { return; }
        let i = state.selected().unwrap_or(0);
        state.select(Some(if i == 0 { list_len - 1 } else { i - 1 }));
    }

    pub fn scroll_down(&mut self) {
        let (list_len, state) = match self.active {
            Focus::Recent => (self.recent.len(), &mut self.recent_state),
            Focus::Neighbors => (self.neighbors.len(), &mut self.neighbor_state),
        };
        if list_len == 0 { return; }
        let i = state.selected().unwrap_or(0);
        state.select(Some((i + 1) % list_len));
    }

    pub fn toggle_focus(&mut self) {
        self.active = match self.active {
            Focus::Recent => Focus::Neighbors,
            Focus::Neighbors => Focus::Recent,
        };
        if self.active == Focus::Neighbors && self.neighbor_state.selected().is_none() && !self.neighbors.is_empty() {
            self.neighbor_state.select(Some(0));
        }
    }

    /// Enter — re-center on whatever is currently selected.
    pub async fn recenter(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let new_focus_id = match self.active {
            Focus::Recent => self.recent_state.selected()
                .and_then(|i| self.recent.get(i))
                .map(|n| (n.id, short(&n.snippet, 60))),
            Focus::Neighbors => self.neighbor_state.selected()
                .and_then(|i| self.neighbors.get(i))
                .map(|n| (n.id, short(&n.snippet, 60))),
        };
        if let Some((id, label)) = new_focus_id {
            self.focus_id = Some(id);
            self.focus_label = label;
            self.refresh_neighbors(pool).await;
            self.active = Focus::Neighbors;
            if !self.neighbors.is_empty() {
                self.neighbor_state.select(Some(0));
            }
        }
        Ok(())
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        // Recent high-signal nodes across all agents — embedded only, since
        // we'll want to query their neighborhoods.
        let rows = sqlx::query(
            r#"
            SELECT n.id, n.kind::text AS kind, n.created_at, n.content,
                   COALESCE(a.agent_name, '') AS agent_name
            FROM nodes n
            LEFT JOIN agents a ON a.agent_id = n.agent_id
            WHERE n.embedding IS NOT NULL
              AND n.kind IN ('directive', 'digest', 'user_message')
            ORDER BY n.created_at DESC
            LIMIT 40
            "#
        ).fetch_all(pool).await.unwrap_or_default();

        self.recent = rows.into_iter().map(|r| {
            let content: serde_json::Value = r.try_get("content").unwrap_or(serde_json::Value::Null);
            MemNode {
                id: r.get("id"),
                kind: r.get("kind"),
                agent_name: r.get("agent_name"),
                created_at: r.get("created_at"),
                snippet: extract_snippet(&content),
            }
        }).collect();

        if self.recent.is_empty() {
            self.last_status = "no embedded nodes yet — write directives or run sessions".into();
        } else {
            self.last_status.clear();
        }

        // Default focus to newest if nothing is set.
        if self.focus_id.is_none() {
            if let Some(first) = self.recent.first() {
                self.focus_id = Some(first.id);
                self.focus_label = short(&first.snippet, 60);
                self.refresh_neighbors(pool).await;
            }
        }
        Ok(())
    }

    async fn refresh_neighbors(&mut self, pool: &PgPool) {
        let Some(focus_id) = self.focus_id else { return; };

        // Pull the focus node's embedding, then find its 8 nearest neighbors
        // (excluding itself) across the whole cloud.
        let focus_row = sqlx::query("SELECT embedding FROM nodes WHERE id = $1")
            .bind(focus_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
        let Some(row) = focus_row else {
            self.neighbors.clear();
            return;
        };
        let embedding: Option<Vector> = row.try_get("embedding").ok();
        let Some(embedding) = embedding else {
            self.neighbors.clear();
            return;
        };

        let rows = sqlx::query(
            r#"
            SELECT n.id, n.kind::text AS kind, n.created_at, n.content,
                   COALESCE(a.agent_name, '') AS agent_name,
                   (n.embedding <=> $1)::float8 AS distance
            FROM nodes n
            LEFT JOIN agents a ON a.agent_id = n.agent_id
            WHERE n.embedding IS NOT NULL
              AND n.id <> $2
            ORDER BY n.embedding <=> $1
            LIMIT 8
            "#
        )
        .bind(&embedding)
        .bind(focus_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        self.neighbors = rows.into_iter().map(|r| {
            let content: serde_json::Value = r.try_get("content").unwrap_or(serde_json::Value::Null);
            let distance: f64 = r.try_get("distance").unwrap_or(1.0);
            Neighbor {
                id: r.get("id"),
                kind: r.get("kind"),
                agent_name: r.get("agent_name"),
                similarity: (1.0 - distance).clamp(0.0, 1.0),
                snippet: extract_snippet(&content),
                created_at: r.get("created_at"),
            }
        }).collect();
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Three rows: stats strip / recent list / neighbors.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),          // stats strip
                Constraint::Percentage(50),     // recent
                Constraint::Percentage(50),     // neighbors
            ])
            .split(area);

        self.render_stats(frame, chunks[0]);
        self.render_recent(frame, chunks[1]);
        self.render_neighbors(frame, chunks[2]);
    }

    fn render_stats(&self, frame: &mut Frame, area: Rect) {
        let neighbor_count = self.neighbors.len();
        let (min_sim, max_sim, mean_sim) = if neighbor_count > 0 {
            let sims: Vec<f64> = self.neighbors.iter().map(|n| n.similarity).collect();
            let mn = sims.iter().cloned().fold(f64::INFINITY, f64::min);
            let mx = sims.iter().cloned().fold(0.0_f64, f64::max);
            let mean = sims.iter().sum::<f64>() / sims.len() as f64;
            (mn, mx, mean)
        } else { (0.0, 0.0, 0.0) };

        let active_label = match self.active {
            Focus::Recent => "recent",
            Focus::Neighbors => "neighbors",
        };

        let focus_line = if self.focus_id.is_some() {
            format!("focus: {}", short(&self.focus_label, 70))
        } else {
            "focus: (none — Enter on a node to recenter)".to_string()
        };

        let line1 = Line::from(vec![
            Span::styled("  active: ", Style::default().fg(Color::DarkGray)),
            Span::styled(active_label, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled("  ·  space=switch  ·  Enter=recenter  ·  Esc=back to recent",
                Style::default().fg(Color::DarkGray)),
        ]);
        let line2 = if neighbor_count > 0 {
            Line::from(vec![
                Span::styled(format!("  {}  ·  ", focus_line), Style::default().fg(Color::Cyan)),
                Span::styled(format!("neighbors: {neighbor_count}  "),
                    Style::default().fg(Color::DarkGray)),
                Span::styled(format!("sim min {:.0}% / mean {:.0}% / max {:.0}%",
                    min_sim*100.0, mean_sim*100.0, max_sim*100.0),
                    Style::default().fg(Color::Green)),
            ])
        } else {
            Line::from(Span::styled(format!("  {focus_line}"), Style::default().fg(Color::DarkGray)))
        };

        let para = Paragraph::new(vec![line1, line2])
            .block(Block::default().borders(Borders::ALL).title(" Memgraph "));
        frame.render_widget(para, area);
    }

    /// Esc — return to recent pane without recentering.
    pub fn back_to_recent(&mut self) {
        self.active = Focus::Recent;
    }

    fn render_recent(&mut self, frame: &mut Frame, area: Rect) {
        if self.recent.is_empty() {
            let msg = if self.last_status.is_empty() {
                "loading…".to_string()
            } else { self.last_status.clone() };
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(Color::DarkGray),
                )),
            ]).block(Block::default().borders(Borders::ALL)
                .title(" Recent high-signal nodes — Tab to switch panes · Enter to recenter "));
            frame.render_widget(para, area);
            return;
        }

        let items: Vec<ListItem> = self.recent.iter().map(|n| {
            let (glyph, color) = kind_style(&n.kind);
            let age = humanize_since(n.created_at);
            let is_focus = Some(n.id) == self.focus_id;
            let focus_marker = if is_focus { "◎ " } else { "  " };
            ListItem::new(Line::from(vec![
                Span::styled(focus_marker.to_string(), Style::default().fg(Color::Magenta)),
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(format!("{:<10}", n.kind), Style::default().fg(color)),
                Span::styled(format!(" {:<12}", short(&n.agent_name, 12)),
                    Style::default().fg(Color::Cyan)),
                Span::styled(format!(" {:<6}", age), Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::raw(short(&n.snippet, 70)),
            ]))
        }).collect();

        let title = format!(" Recent high-signal nodes ({}){} ",
            self.recent.len(),
            if self.active == Focus::Recent { " · [active] Tab=switch · Enter=recenter" }
            else { " · Tab=switch" });

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL)
                .title(title)
                .border_style(if self.active == Focus::Recent {
                    Style::default().fg(Color::Cyan)
                } else { Style::default().fg(Color::DarkGray) }))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
        frame.render_stateful_widget(list, area, &mut self.recent_state);
    }

    fn render_neighbors(&mut self, frame: &mut Frame, area: Rect) {
        let focus_summary = if self.focus_id.is_some() {
            format!(" focus: {} ", short(&self.focus_label, 60))
        } else {
            " no focus yet — select a node above, press Enter ".to_string()
        };

        if self.neighbors.is_empty() {
            let msg: &str = if self.focus_id.is_some() {
                "no neighbors found — node may lack an embedding"
            } else {
                "select a node above and press Enter to recenter"
            };
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(Color::DarkGray),
                )),
            ]).block(Block::default().borders(Borders::ALL)
                .title(format!(" Neighbors · {} ", focus_summary)));
            frame.render_widget(para, area);
            return;
        }

        let items: Vec<ListItem> = self.neighbors.iter().map(|n| {
            let (glyph, color) = kind_style(&n.kind);
            let sim_pct = (n.similarity * 100.0) as u32;
            let bar_blocks = (sim_pct / 10).min(10) as usize;
            let bar = format!("{}{}",
                "█".repeat(bar_blocks),
                "░".repeat(10 - bar_blocks));
            let sim_color = if sim_pct >= 80 { Color::Green }
                            else if sim_pct >= 60 { Color::Cyan }
                            else if sim_pct >= 40 { Color::Yellow }
                            else { Color::DarkGray };
            let age = humanize_since(n.created_at);

            ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{bar} "), Style::default().fg(sim_color)),
                Span::styled(format!("{sim_pct:>3}%"),
                    Style::default().fg(sim_color).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(format!("{:<10}", n.kind), Style::default().fg(color)),
                Span::styled(format!(" {:<12}", short(&n.agent_name, 12)),
                    Style::default().fg(Color::Cyan)),
                Span::styled(format!(" {:<6}", age), Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::raw(short(&n.snippet, 60)),
            ]))
        }).collect();

        let title = format!(" Neighbors · {}{} ", focus_summary,
            if self.active == Focus::Neighbors { " · [active] Enter=recenter" }
            else { "" });

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL)
                .title(title)
                .border_style(if self.active == Focus::Neighbors {
                    Style::default().fg(Color::Cyan)
                } else { Style::default().fg(Color::DarkGray) }))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
        frame.render_stateful_widget(list, area, &mut self.neighbor_state);
    }
}

fn kind_style(kind: &str) -> (&'static str, Color) {
    match kind {
        "directive" => ("♦", Color::Blue),
        "digest" => ("◈", Color::Cyan),
        "user_message" => ("●", Color::Green),
        "assistant_message" => ("○", Color::White),
        "tool_call" => ("▸", Color::Yellow),
        "tool_result" => ("◂", Color::DarkGray),
        "system" => ("▪", Color::Magenta),
        "human_override" => ("!", Color::Red),
        _ => ("·", Color::Gray),
    }
}

fn extract_snippet(content: &serde_json::Value) -> String {
    // Try common shapes in priority order.
    for key in ["snippet", "text", "summary", "prompt", "body", "message"] {
        if let Some(s) = content.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() { return s.replace('\n', " "); }
        }
    }
    // Fall back to any string value in the object.
    if let Some(obj) = content.as_object() {
        for (_, v) in obj {
            if let Some(s) = v.as_str() {
                if !s.is_empty() { return s.replace('\n', " "); }
            }
        }
    }
    content.to_string()
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    s.chars().take(max).collect::<String>() + "…"
}

fn humanize_since(ts: DateTime<Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0);
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else if secs < 86400 { format!("{}h", secs / 3600) }
    else { format!("{}d", secs / 86400) }
}
