//! Memgraph pane — memory similarity explorer.
//!
//! Top half: recent high-signal nodes (directive / digest / user_message)
//! with KIND, AGENT, AGE, SNIPPET columns. Cursor moves through this list
//! with arrow keys; Neighbors live-follow the highlighted row.
//! Bottom half: top-8 cosine neighbors of whatever's highlighted above,
//! with SIM bar, DIST, KIND, AGENT, AGE, SNIPPET. Informational — no
//! cursor. Enter on a Recent row opens a detail overlay with full text.
//! Esc closes the overlay.

use chrono::{DateTime, Utc};
use pgvector::Vector;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use sqlx::{PgPool, Row as SqlxRow};
use uuid::Uuid;

use crate::embed::Embedder;
use crate::models::node::NodeKind;

#[derive(Debug, Clone, PartialEq)]
pub enum MemSource {
    Node,
    Memory { pinned: bool, scope: String },
}

#[derive(Debug, Clone)]
pub struct MemNode {
    pub id: Uuid,
    pub kind: String,
    pub agent_name: String,
    pub created_at: DateTime<Utc>,
    pub snippet: String,
    pub full_text: String,
    pub source: MemSource,
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

pub struct MemGraphView {
    pub recent: Vec<MemNode>,
    pub recent_state: TableState,
    pub neighbors: Vec<Neighbor>,
    pub last_status: String,
    pub detail_open: bool,
    /// Which recent row's neighbors are currently loaded. Tracks the cursor
    /// so we don't re-query on every render tick — only when it moved.
    loaded_for: Option<Uuid>,
    /// Search mode: when Some, the bottom panel shows search results instead
    /// of neighbors. `/` enters, Esc exits, Enter runs.
    pub search_input: Option<String>,
    pub search_status: String,
    pub search_results: Vec<Neighbor>,
}

impl MemGraphView {
    pub fn new() -> Self {
        let mut s = TableState::default();
        s.select(Some(0));
        Self {
            recent: vec![],
            recent_state: s,
            neighbors: vec![],
            last_status: String::new(),
            detail_open: false,
            loaded_for: None,
            search_input: None,
            search_status: String::new(),
            search_results: vec![],
        }
    }

    pub fn scroll_up(&mut self) {
        if self.recent.is_empty() {
            return;
        }
        let i = self.recent_state.selected().unwrap_or(0);
        let n = self.recent.len();
        self.recent_state
            .select(Some(if i == 0 { n - 1 } else { i - 1 }));
    }

    pub fn scroll_down(&mut self) {
        if self.recent.is_empty() {
            return;
        }
        let i = self.recent_state.selected().unwrap_or(0);
        self.recent_state.select(Some((i + 1) % self.recent.len()));
    }

    pub fn search_mode(&self) -> bool {
        self.search_input.is_some()
    }

    pub fn search_begin(&mut self) {
        self.search_input = Some(String::new());
        self.search_status = "type query, Enter to search".into();
    }

    pub fn search_cancel(&mut self) {
        self.search_input = None;
        self.search_results.clear();
        self.search_status.clear();
    }

    pub fn search_push(&mut self, c: char) {
        if let Some(ref mut buf) = self.search_input {
            buf.push(c);
        }
    }

    pub fn search_pop(&mut self) {
        if let Some(ref mut buf) = self.search_input {
            buf.pop();
        }
    }

    pub async fn search_run(&mut self, pool: &PgPool) {
        let query = match &self.search_input {
            Some(q) if !q.trim().is_empty() => q.trim().to_string(),
            _ => {
                self.search_status = "empty query".into();
                self.search_results.clear();
                return;
            }
        };
        self.search_status = "embedding...".into();
        let embedder = Embedder::default_ollama();
        if !embedder.health_check().await {
            self.search_status = "ollama unavailable".into();
            return;
        }
        let start = std::time::Instant::now();
        let (vec, cached) = match embedder.embed_cached(pool, &query).await {
            Ok(v) => v,
            Err(e) => {
                self.search_status = format!("embed failed: {e}");
                return;
            }
        };
        let node_repo = crate::models::node::NodeRepo::new(pool);
        let kinds = [NodeKind::UserMessage, NodeKind::Directive, NodeKind::Digest];
        let hits = match node_repo
            .hybrid_search_global(&vec, &query, &kinds, 12, 0.8)
            .await
        {
            Ok(h) => h,
            Err(_) => node_repo
                .similarity_search_global(&vec, &kinds, 12, 0.8)
                .await
                .unwrap_or_default(),
        };
        let elapsed = start.elapsed();
        self.search_status = format!(
            "{} result(s) in {:.0}ms {}",
            hits.len(),
            elapsed.as_secs_f64() * 1000.0,
            if cached { "(cached)" } else { "(fresh)" }
        );
        self.search_results = hits
            .into_iter()
            .map(|h| {
                let snippet_text = h
                    .content
                    .get("text")
                    .or_else(|| h.content.get("directive"))
                    .or_else(|| h.content.get("summary"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no text)")
                    .replace('\n', " ");
                let sim = h.similarity();
                Neighbor {
                    id: h.id,
                    kind: format!("{:?}", h.kind).to_lowercase(),
                    agent_name: h.agent_name,
                    similarity: sim,
                    snippet: snippet_text,
                    created_at: h.created_at,
                }
            })
            .collect();
    }

    pub fn toggle_detail(&mut self) {
        if self.recent.is_empty() {
            return;
        }
        self.detail_open = !self.detail_open;
    }

    fn selected_node(&self) -> Option<&MemNode> {
        let i = self.recent_state.selected()?;
        self.recent.get(i)
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        // Nodes (transcript-level memory).
        let node_rows = sqlx::query(
            r#"
            SELECT n.id, n.kind::text AS kind, n.created_at, n.content,
                   COALESCE(a.agent_name, '') AS agent_name
            FROM nodes n
            LEFT JOIN agents a ON a.agent_id = n.agent_id
            WHERE n.embedding IS NOT NULL
              AND n.kind IN ('directive', 'digest', 'user_message')
            ORDER BY n.created_at DESC
            LIMIT 40
            "#,
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let mut combined: Vec<MemNode> = node_rows
            .into_iter()
            .map(|r| {
                let content: serde_json::Value =
                    r.try_get("content").unwrap_or(serde_json::Value::Null);
                let full_text = extract_full_text(&content);
                let snippet = full_text.replace('\n', " ");
                MemNode {
                    id: r.get("id"),
                    kind: r.get("kind"),
                    agent_name: r.get("agent_name"),
                    created_at: r.get("created_at"),
                    snippet,
                    full_text,
                    source: MemSource::Node,
                }
            })
            .collect();

        // Memories (first-class scoped notes). Surface them alongside nodes.
        let mem_rows = sqlx::query(
            r#"
            SELECT memory_id, scope::text AS scope, created_at, text, agent_name, pinned
            FROM memories
            WHERE embedding IS NOT NULL
              AND (expires_at IS NULL OR expires_at > now())
            ORDER BY pinned DESC, created_at DESC
            LIMIT 40
            "#,
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        for r in mem_rows {
            let text: String = r.get("text");
            let scope: String = r.get("scope");
            let pinned: bool = r.get("pinned");
            let snippet = text.replace('\n', " ");
            combined.push(MemNode {
                id: r.get("memory_id"),
                kind: "memory".to_string(),
                agent_name: r.get("agent_name"),
                created_at: r.get("created_at"),
                snippet,
                full_text: text,
                source: MemSource::Memory { pinned, scope },
            });
        }

        // Merge order: pinned memories first, then everything by created_at DESC.
        combined.sort_by(|a, b| {
            let a_pin = matches!(&a.source, MemSource::Memory { pinned: true, .. });
            let b_pin = matches!(&b.source, MemSource::Memory { pinned: true, .. });
            b_pin.cmp(&a_pin).then(b.created_at.cmp(&a.created_at))
        });
        combined.truncate(40);
        self.recent = combined;

        if self.recent.is_empty() {
            self.last_status = "no embedded nodes yet — write directives or run sessions".into();
        } else {
            self.last_status.clear();
        }

        // Pin the cursor inside the list if it ran off.
        if let Some(i) = self.recent_state.selected() {
            if i >= self.recent.len() {
                self.recent_state.select(if self.recent.is_empty() {
                    None
                } else {
                    Some(self.recent.len() - 1)
                });
            }
        }

        // Refresh neighbors for the currently highlighted row if that row
        // changed since the last tick. Cheap — single HNSW query.
        let target = self.selected_node().map(|n| n.id);
        if target != self.loaded_for {
            self.loaded_for = target;
            if let Some(id) = target {
                self.refresh_neighbors(pool, id).await;
            } else {
                self.neighbors.clear();
            }
        }
        Ok(())
    }

    async fn refresh_neighbors(&mut self, pool: &PgPool, for_id: Uuid) {
        // The focus id could live in either nodes or memories — try both.
        let embedding: Option<Vector> = {
            let node = sqlx::query("SELECT embedding FROM nodes WHERE id = $1")
                .bind(for_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
            if let Some(r) = node {
                r.try_get("embedding").ok()
            } else {
                sqlx::query("SELECT embedding FROM memories WHERE memory_id = $1")
                    .bind(for_id)
                    .fetch_optional(pool)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|r| r.try_get("embedding").ok())
            }
        };
        let Some(embedding) = embedding else {
            self.neighbors.clear();
            return;
        };

        // Union neighbors from nodes + memories. Postgres sorts the whole
        // set by distance; we take the top 8 across both sources.
        let rows = sqlx::query(
            r#"
            (
              SELECT n.id AS id, n.kind::text AS kind, n.created_at,
                     n.content::text AS content_text,
                     COALESCE(a.agent_name, '') AS agent_name,
                     (n.embedding <=> $1)::float8 AS distance
              FROM nodes n
              LEFT JOIN agents a ON a.agent_id = n.agent_id
              WHERE n.embedding IS NOT NULL AND n.id <> $2
            )
            UNION ALL
            (
              SELECT memory_id AS id, 'memory'::text AS kind, created_at,
                     text AS content_text,
                     agent_name,
                     (embedding <=> $1)::float8 AS distance
              FROM memories
              WHERE embedding IS NOT NULL AND memory_id <> $2
                AND (expires_at IS NULL OR expires_at > now())
            )
            ORDER BY distance
            LIMIT 8
            "#,
        )
        .bind(&embedding)
        .bind(for_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        self.neighbors = rows
            .into_iter()
            .map(|r| {
                let kind: String = r.get("kind");
                let content_text: String = r.try_get("content_text").unwrap_or_default();
                let distance: f64 = r.try_get("distance").unwrap_or(1.0);
                let snippet = if kind == "memory" {
                    content_text.replace('\n', " ")
                } else {
                    // Nodes store content as JSON; pull the most useful field.
                    serde_json::from_str::<serde_json::Value>(&content_text)
                        .ok()
                        .map(|v| extract_snippet(&v))
                        .unwrap_or(content_text)
                };
                Neighbor {
                    id: r.get("id"),
                    kind,
                    agent_name: r.get("agent_name"),
                    similarity: (1.0 - distance).clamp(0.0, 1.0),
                    snippet,
                    created_at: r.get("created_at"),
                }
            })
            .collect();
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if self.search_mode() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Percentage(50),
                    Constraint::Percentage(50),
                ])
                .split(area);

            self.render_search_bar(frame, chunks[0]);
            self.render_recent(frame, chunks[1]);
            self.render_search_results(frame, chunks[2]);
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Percentage(50),
                    Constraint::Percentage(50),
                ])
                .split(area);

            self.render_stats(frame, chunks[0]);
            self.render_recent(frame, chunks[1]);
            self.render_neighbors(frame, chunks[2]);
        }

        if self.detail_open {
            if let Some(n) = self.selected_node().cloned() {
                render_detail_overlay(frame, area, &n);
            }
        }
    }

    fn render_stats(&self, frame: &mut Frame, area: Rect) {
        let neighbor_count = self.neighbors.len();
        let (min_sim, max_sim, mean_sim) = if neighbor_count > 0 {
            let sims: Vec<f64> = self.neighbors.iter().map(|n| n.similarity).collect();
            let mn = sims.iter().cloned().fold(f64::INFINITY, f64::min);
            let mx = sims.iter().cloned().fold(0.0_f64, f64::max);
            let mean = sims.iter().sum::<f64>() / sims.len() as f64;
            (mn, mx, mean)
        } else {
            (0.0, 0.0, 0.0)
        };

        let focus_line = match self.selected_node() {
            Some(n) => format!("cursor: {}", short(&n.snippet, 70)),
            None => "cursor: (empty list)".to_string(),
        };

        let line1 = Line::from(vec![Span::styled(
            "  ↑↓ scroll  ·  Enter=detail  ·  Esc=close",
            Style::default().fg(Color::DarkGray),
        )]);
        let line2 = if neighbor_count > 0 {
            Line::from(vec![
                Span::styled(
                    format!("  {}  ·  ", focus_line),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("neighbors: {neighbor_count}  "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!(
                        "sim min {:.0}% / mean {:.0}% / max {:.0}%",
                        min_sim * 100.0,
                        mean_sim * 100.0,
                        max_sim * 100.0
                    ),
                    Style::default().fg(Color::Green),
                ),
            ])
        } else {
            Line::from(Span::styled(
                format!("  {focus_line}"),
                Style::default().fg(Color::DarkGray),
            ))
        };

        let para = Paragraph::new(vec![line1, line2])
            .block(Block::default().borders(Borders::ALL).title(" Memgraph "));
        frame.render_widget(para, area);
    }

    fn render_recent(&mut self, frame: &mut Frame, area: Rect) {
        if self.recent.is_empty() {
            let msg = if self.last_status.is_empty() {
                "loading…".to_string()
            } else {
                self.last_status.clone()
            };
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Recent high-signal nodes "),
            );
            frame.render_widget(para, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("KIND").style(Style::default().fg(Color::Gray)),
            Cell::from("AGENT").style(Style::default().fg(Color::Gray)),
            Cell::from("AGE").style(Style::default().fg(Color::Gray)),
            Cell::from("SNIPPET").style(Style::default().fg(Color::Gray)),
        ]);
        let rows: Vec<Row> = self
            .recent
            .iter()
            .map(|n| {
                let (glyph, color, label) = match &n.source {
                    MemSource::Memory {
                        pinned: true,
                        scope,
                    } => ("★", Color::Yellow, format!("memory:{scope}")),
                    MemSource::Memory {
                        pinned: false,
                        scope,
                    } => ("♦", Color::Blue, format!("memory:{scope}")),
                    MemSource::Node => {
                        let (g, c) = kind_style(&n.kind);
                        (g, c, n.kind.clone())
                    }
                };
                let age = humanize_since(n.created_at);
                Row::new(vec![
                    Cell::from(format!("{glyph} {}", label)).style(Style::default().fg(color)),
                    Cell::from(short(&n.agent_name, 16)).style(Style::default().fg(Color::Cyan)),
                    Cell::from(age).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(short(&n.snippet, 120)),
                ])
            })
            .collect();

        let mem_count = self
            .recent
            .iter()
            .filter(|n| matches!(n.source, MemSource::Memory { .. }))
            .count();
        let node_count = self.recent.len() - mem_count;
        let title = format!(
            " Recent: {} nodes + {} memories  ·  Enter=detail ",
            node_count, mem_count
        );
        let table = Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Length(18),
                Constraint::Length(6),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(table, area, &mut self.recent_state);
    }

    fn render_neighbors(&self, frame: &mut Frame, area: Rect) {
        let focus_summary = match self.selected_node() {
            Some(n) => format!("neighbors of: {}", short(&n.snippet, 70)),
            None => "no row highlighted".to_string(),
        };

        if self.neighbors.is_empty() {
            let msg = if self.selected_node().is_some() {
                "no neighbors found — node may lack an embedding"
            } else {
                "highlight a row above"
            };
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {focus_summary} ")),
            );
            frame.render_widget(para, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("SIM").style(Style::default().fg(Color::Gray)),
            Cell::from("DIST").style(Style::default().fg(Color::Gray)),
            Cell::from("KIND").style(Style::default().fg(Color::Gray)),
            Cell::from("AGENT").style(Style::default().fg(Color::Gray)),
            Cell::from("AGE").style(Style::default().fg(Color::Gray)),
            Cell::from("SNIPPET").style(Style::default().fg(Color::Gray)),
        ]);
        let rows: Vec<Row> = self
            .neighbors
            .iter()
            .map(|n| {
                let (glyph, color) = kind_style(&n.kind);
                let sim_pct = (n.similarity * 100.0) as u32;
                let distance = 1.0 - n.similarity;
                let bar_blocks = (sim_pct / 10).min(10) as usize;
                let bar = format!(
                    "{}{}  {sim_pct:>3}%",
                    "█".repeat(bar_blocks),
                    "░".repeat(10 - bar_blocks)
                );
                let sim_color = if sim_pct >= 80 {
                    Color::Green
                } else if sim_pct >= 60 {
                    Color::Cyan
                } else if sim_pct >= 40 {
                    Color::Yellow
                } else {
                    Color::DarkGray
                };
                let age = humanize_since(n.created_at);
                Row::new(vec![
                    Cell::from(bar).style(Style::default().fg(sim_color)),
                    Cell::from(format!("{:.3}", distance))
                        .style(Style::default().fg(Color::DarkGray)),
                    Cell::from(format!("{glyph} {}", n.kind)).style(Style::default().fg(color)),
                    Cell::from(short(&n.agent_name, 16)).style(Style::default().fg(Color::Cyan)),
                    Cell::from(age).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(short(&n.snippet, 120)),
                ])
            })
            .collect();

        let title = format!(" {focus_summary} ");
        let table = Table::new(
            rows,
            [
                Constraint::Length(18),
                Constraint::Length(6),
                Constraint::Length(18),
                Constraint::Length(18),
                Constraint::Length(6),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(table, area);
    }

    fn render_search_bar(&self, frame: &mut Frame, area: Rect) {
        let query_text = self.search_input.as_deref().unwrap_or("");
        let title = format!(
            " Search — {}  (/=search  Enter=run  Esc=cancel) ",
            self.search_status
        );
        let para = Paragraph::new(query_text.to_string())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::Magenta)),
            )
            .style(Style::default().fg(Color::White));
        frame.render_widget(para, area);
    }

    fn render_search_results(&self, frame: &mut Frame, area: Rect) {
        if self.search_results.is_empty() {
            let msg = if self.search_status.is_empty() {
                "type a query and press Enter"
            } else {
                &self.search_status
            };
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Search results ")
                    .border_style(Style::default().fg(Color::Magenta)),
            );
            frame.render_widget(para, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("SIM").style(Style::default().fg(Color::Gray)),
            Cell::from("KIND").style(Style::default().fg(Color::Gray)),
            Cell::from("AGENT").style(Style::default().fg(Color::Gray)),
            Cell::from("AGE").style(Style::default().fg(Color::Gray)),
            Cell::from("SNIPPET").style(Style::default().fg(Color::Gray)),
        ]);
        let rows: Vec<Row> = self
            .search_results
            .iter()
            .map(|n| {
                let (glyph, color) = kind_style(&n.kind);
                let sim_pct = (n.similarity * 100.0) as u32;
                let bar_blocks = (sim_pct / 10).min(10) as usize;
                let bar = format!(
                    "{}{}  {sim_pct:>3}%",
                    "█".repeat(bar_blocks),
                    "░".repeat(10 - bar_blocks)
                );
                let sim_color = if sim_pct >= 80 {
                    Color::Green
                } else if sim_pct >= 60 {
                    Color::Cyan
                } else if sim_pct >= 40 {
                    Color::Yellow
                } else {
                    Color::DarkGray
                };
                let age = humanize_since(n.created_at);
                Row::new(vec![
                    Cell::from(bar).style(Style::default().fg(sim_color)),
                    Cell::from(format!("{glyph} {}", n.kind)).style(Style::default().fg(color)),
                    Cell::from(short(&n.agent_name, 16)).style(Style::default().fg(Color::Cyan)),
                    Cell::from(age).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(short(&n.snippet, 120)),
                ])
            })
            .collect();

        let title = format!(" {} search results ", self.search_results.len());
        let table = Table::new(
            rows,
            [
                Constraint::Length(18),
                Constraint::Length(18),
                Constraint::Length(18),
                Constraint::Length(6),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Magenta)),
        );
        frame.render_widget(table, area);
    }
}

fn render_detail_overlay(frame: &mut Frame, area: Rect, node: &MemNode) {
    let popup_w = area.width.saturating_sub(8).min(110);
    let popup_h = area.height.saturating_sub(4).min(32);
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    frame.render_widget(Clear, popup);

    let (glyph, color) = kind_style(&node.kind);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!(" {glyph} {} ", node.kind),
                Style::default()
                    .fg(Color::Black)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(node.agent_name.clone(), Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(
                humanize_since(node.created_at),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(
                node.id.to_string()[..8].to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
    ];
    for l in node.full_text.lines() {
        lines.push(Line::from(l.to_string()));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" detail — Enter/Esc to close ")
        .border_style(Style::default().fg(Color::Cyan));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
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
    for key in ["snippet", "text", "summary", "prompt", "body", "message"] {
        if let Some(s) = content.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.replace('\n', " ");
            }
        }
    }
    if let Some(obj) = content.as_object() {
        for (_, v) in obj {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return s.replace('\n', " ");
                }
            }
        }
    }
    content.to_string()
}

/// Like extract_snippet, but preserve newlines for the detail overlay.
fn extract_full_text(content: &serde_json::Value) -> String {
    for key in ["text", "summary", "prompt", "body", "message", "snippet"] {
        if let Some(s) = content.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    if let Some(obj) = content.as_object() {
        for (_, v) in obj {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string())
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

fn humanize_since(ts: DateTime<Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}
