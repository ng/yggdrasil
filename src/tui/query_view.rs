//! Query console — type a query, hit Enter, see the semantic+lexical
//! retrieval result live. Great for probing "does the system understand
//! what I think this text means?"

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use sqlx::PgPool;

use crate::embed::Embedder;
use crate::models::node::{NodeKind, SearchHit};

pub struct QueryView {
    pub input: String,
    pub results: Vec<SearchHit>,
    pub state: ListState,
    pub status: String,
}

impl QueryView {
    pub fn new() -> Self {
        let mut st = ListState::default();
        st.select(Some(0));
        Self {
            input: String::new(),
            results: vec![],
            state: st,
            status: "type a query, Enter to run · Esc/Tab to leave".into(),
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
    }

    pub fn pop_char(&mut self) {
        self.input.pop();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
    }

    pub fn select_prev(&mut self) {
        if self.results.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(if i == 0 {
            self.results.len() - 1
        } else {
            i - 1
        }));
    }

    pub fn select_next(&mut self) {
        if self.results.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1) % self.results.len()));
    }

    pub async fn run_query(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        self.status = "embedding...".into();
        let q = self.input.trim();
        if q.is_empty() {
            self.results.clear();
            self.status = "empty query".into();
            return Ok(());
        }
        let embedder = Embedder::default_ollama();
        if !embedder.health_check().await {
            self.status = "ollama unavailable".into();
            return Ok(());
        }
        let start = std::time::Instant::now();
        let (vec, cached) = match embedder.embed_cached(pool, q).await {
            Ok(v) => v,
            Err(e) => {
                self.status = format!("embed failed: {e}");
                return Ok(());
            }
        };
        let node_repo = crate::models::node::NodeRepo::new(pool);
        let kinds = [NodeKind::UserMessage, NodeKind::Directive, NodeKind::Digest];
        let results = match node_repo
            .hybrid_search_global(&vec, q, &kinds, 12, 0.8)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                debug_fallback(&mut self.status, &e);
                node_repo
                    .similarity_search_global(&vec, &kinds, 12, 0.8)
                    .await?
            }
        };
        let elapsed = start.elapsed();
        self.status = format!(
            "{} result(s) in {:.0}ms  {}",
            results.len(),
            elapsed.as_secs_f64() * 1000.0,
            if cached {
                "(embed cached)"
            } else {
                "(embed fresh)"
            }
        );
        self.results = results;
        self.state.select(if self.results.is_empty() {
            None
        } else {
            Some(0)
        });
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Layout: input line + results list (left) + detail (right).
        let top_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let input_widget = Paragraph::new(self.input.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Query — {}  (type, Enter to run) ", self.status)),
            )
            .style(Style::default().fg(Color::White));
        frame.render_widget(input_widget, top_split[0]);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(top_split[1]);

        // Results list
        let items: Vec<ListItem> = self
            .results
            .iter()
            .map(|h| {
                let sim = h.similarity();
                let snip = extract_snippet(&h.content);
                let kind = format!("{:?}", h.kind).to_lowercase();
                let (label, color) = if sim >= 0.6 {
                    ("strong", Color::Green)
                } else if sim >= 0.4 {
                    ("recall", Color::Blue)
                } else {
                    ("faint", Color::DarkGray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(label, Style::default().fg(color)),
                    Span::raw(format!(" {:.2} ", sim)),
                    Span::styled(
                        format!("{:<18}", truncate(&h.agent_name, 18)),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!(" [{kind:<8}] "),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(truncate(&snip, 40).to_string()),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Results "))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, body[0], &mut self.state);

        // Detail panel for selected result
        let detail_lines: Vec<Line<'static>> = if let Some(i) = self.state.selected() {
            if let Some(hit) = self.results.get(i) {
                build_detail(hit)
            } else {
                vec![]
            }
        } else {
            vec![Line::from("select a result")]
        };

        let detail = Paragraph::new(detail_lines)
            .block(Block::default().borders(Borders::ALL).title(" Detail "))
            .wrap(Wrap { trim: false });
        frame.render_widget(detail, body[1]);
    }
}

fn debug_fallback(status: &mut String, e: &sqlx::Error) {
    *status = format!("hybrid failed ({e}), fell back to vector-only");
}

fn build_detail(hit: &SearchHit) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(vec![
        Span::styled("agent   ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            hit.agent_name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    out.push(Line::from(vec![
        Span::styled("kind    ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{:?}", hit.kind).to_lowercase()),
    ]));
    out.push(Line::from(vec![
        Span::styled("age     ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_age(hit.created_at)),
    ]));
    out.push(Line::from(vec![
        Span::styled("cosine  ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{:.3}", hit.distance)),
    ]));
    out.push(Line::from(vec![
        Span::styled("sim     ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{:.3}", hit.similarity())),
    ]));
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(
        "content:",
        Style::default().fg(Color::DarkGray),
    )));
    let text = extract_snippet(&hit.content);
    // Split long text into wrappable chunks
    for chunk in text.split('\n') {
        out.push(Line::from(chunk.to_string()));
    }
    out
}

fn extract_snippet(content: &serde_json::Value) -> String {
    content
        .get("text")
        .or_else(|| content.get("directive"))
        .or_else(|| content.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("(no text)")
        .to_string()
}

fn format_age(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - ts).num_seconds();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
