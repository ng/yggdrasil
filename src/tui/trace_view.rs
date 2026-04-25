//! Trace pane — interactive retrieval inspector. Lists recent user
//! prompts in the left panel; selecting one renders the full pipeline
//! tree in the right panel.

use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use sqlx::PgPool;
use uuid::Uuid;

pub struct TraceView {
    pub prompts: Vec<TracePrompt>,
    pub state: ListState,
    pub trace_lines: Vec<Line<'static>>,
}

pub struct TracePrompt {
    pub agent_id: Uuid,
    pub agent_name: String,
    pub ts: DateTime<Utc>,
    pub snippet: String,
}

impl TraceView {
    pub fn new() -> Self {
        let mut st = ListState::default();
        st.select(Some(0));
        Self {
            prompts: vec![],
            state: st,
            trace_lines: vec![],
        }
    }

    pub fn select_prev(&mut self) {
        if self.prompts.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(if i == 0 {
            self.prompts.len() - 1
        } else {
            i - 1
        }));
    }

    pub fn select_next(&mut self) {
        if self.prompts.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1) % self.prompts.len()));
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let rows: Vec<(Uuid, String, DateTime<Utc>, serde_json::Value)> = sqlx::query_as(
            r#"SELECT agent_id, agent_name, created_at, payload
               FROM events
               WHERE event_kind::text = 'node_written'
                 AND payload->>'kind' = 'user_message'
               ORDER BY created_at DESC LIMIT 20"#,
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        self.prompts = rows
            .into_iter()
            .map(|(id, name, ts, payload)| {
                let snippet = payload
                    .get("snippet")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                TracePrompt {
                    agent_id: id,
                    agent_name: name,
                    ts,
                    snippet,
                }
            })
            .collect();

        if self.prompts.is_empty() {
            self.state.select(None);
            self.trace_lines.clear();
            return Ok(());
        }
        if self.state.selected().unwrap_or(0) >= self.prompts.len() {
            self.state.select(Some(0));
        }
        // Build the trace tree for the selected prompt.
        let idx = self.state.selected().unwrap_or(0);
        let prompt = &self.prompts[idx];
        self.trace_lines = build_trace_lines(pool, prompt).await;
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area);

        let items: Vec<ListItem> = self
            .prompts
            .iter()
            .map(|p| {
                let ts =
                    p.ts.with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string();
                let snippet = if p.snippet.len() > 40 {
                    format!("{}…", &p.snippet.chars().take(40).collect::<String>())
                } else {
                    p.snippet.clone()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(ts, Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::styled(p.agent_name.clone(), Style::default().fg(Color::Cyan)),
                    Span::raw("  "),
                    Span::raw(snippet),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Recent prompts "),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, chunks[0], &mut self.state);

        let para = Paragraph::new(self.trace_lines.clone())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Pipeline trace "),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(para, chunks[1]);
    }
}

async fn build_trace_lines(pool: &PgPool, p: &TracePrompt) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("prompt  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("\"{}\"", truncate(&p.snippet, 80)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));

    // Pull events in the prompt's turn: everything from this prompt until
    // the next user prompt from the same agent (or 10 min, whichever first).
    // hit_referenced fires at digest time which can be much later than the
    // similarity_hit — include a wide window so we don't miss it.
    let lo = p.ts - chrono::Duration::seconds(1);
    let hi = p.ts + chrono::Duration::hours(2);
    let events: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT event_kind::text, payload FROM events
         WHERE agent_id = $1 AND created_at >= $2 AND created_at <= $3
         ORDER BY created_at ASC, id ASC",
    )
    .bind(p.agent_id)
    .bind(lo)
    .bind(hi)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let mut embed_ms = None;
    let mut embed_cached = false;
    let mut retrieved = 0;
    let mut dropped = Vec::new();
    let mut hits = Vec::new();
    // Track which source_node_ids have been referenced by later turns so we
    // can mark each hit individually, not just a total.
    let mut referenced_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (kind, payload) in &events {
        match kind.as_str() {
            "embedding_cache_hit" => {
                embed_cached = true;
                embed_ms = payload.get("latency_ms").and_then(|v| v.as_u64());
            }
            "embedding_call" if embed_ms.is_none() => {
                embed_ms = payload.get("latency_ms").and_then(|v| v.as_u64());
            }
            "scoring_decision" => {
                retrieved += 1;
                dropped.push(payload.clone());
            }
            "similarity_hit" => {
                retrieved += 1;
                hits.push(payload.clone());
            }
            "hit_referenced" => {
                if let Some(sid) = payload.get("source_node_id").and_then(|v| v.as_str()) {
                    referenced_ids.insert(sid.to_string());
                }
            }
            _ => {}
        }
    }

    // embed
    let embed_label = if embed_cached { "cache hit" } else { "ollama" };
    lines.push(Line::from(vec![
        Span::raw("├─ "),
        Span::styled("embed", Style::default().fg(Color::Cyan)),
        Span::raw(format!("     {}  {}ms", embed_label, embed_ms.unwrap_or(0))),
    ]));

    // retrieve
    lines.push(Line::from(vec![
        Span::raw("├─ "),
        Span::styled("retrieve", Style::default().fg(Color::Cyan)),
        Span::raw(format!(
            "  hybrid (pgvector + tsvector) → {retrieved} candidate(s)"
        )),
    ]));

    // dropped items
    for d in &dropped {
        let total = d
            .get("components")
            .and_then(|c| c.get("total"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let src = d
            .get("source_agent")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let reason = d.get("drop_reason").and_then(|v| v.as_str()).unwrap_or("");
        let snip = d.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
        lines.push(Line::from(vec![
            Span::raw("│    "),
            Span::styled("✗", Style::default().fg(Color::Red)),
            Span::raw(format!(
                " {total:.2}  from {src}  ({reason})  \"{}\"",
                truncate(snip, 45)
            )),
        ]));
    }

    // emit — each hit gets its own referenced verdict so the user sees
    // which specific injections the LLM actually used.
    let mut ref_count = 0usize;
    lines.push(Line::from(vec![
        Span::raw("├─ "),
        Span::styled("emit", Style::default().fg(Color::Green)),
        Span::raw(format!("      {} line(s) to agent", hits.len())),
    ]));
    for h in &hits {
        let score = h
            .get("total_score")
            .or_else(|| h.get("similarity"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let src = h
            .get("source_agent")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let snip = h.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
        let source_id = h
            .get("source_node_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let (label, color) = if score >= 0.6 {
            ("strong", Color::Green)
        } else if score >= 0.3 {
            ("recall", Color::Blue)
        } else {
            ("faint", Color::DarkGray)
        };
        let was_referenced = !source_id.is_empty() && referenced_ids.contains(source_id);
        if was_referenced {
            ref_count += 1;
        }
        let (verdict_glyph, verdict_color) = if was_referenced {
            ("✓", Color::Green)
        } else {
            ("·", Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::raw("│    "),
            Span::styled(
                verdict_glyph,
                Style::default()
                    .fg(verdict_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(label, Style::default().fg(color)),
            Span::raw(format!(
                " {score:.2}  from {src}  \"{}\"",
                truncate(snip, 50)
            )),
        ]));
    }

    // referenced summary — the headline signal
    lines.push(Line::from(""));
    if hits.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("└─ ", Style::default().fg(Color::DarkGray)),
            Span::styled("referenced", Style::default().fg(Color::DarkGray)),
            Span::raw("  no hits surfaced — nothing to reference"),
        ]));
    } else {
        let rate = (ref_count as f64 / hits.len() as f64 * 100.0) as i64;
        let (rate_color, note) = if ref_count == 0 && referenced_ids.is_empty() {
            (
                Color::DarkGray,
                "  (digest hasn't scored this turn yet)".to_string(),
            )
        } else if rate >= 50 {
            (Color::Green, String::new())
        } else if rate >= 20 {
            (Color::Yellow, String::new())
        } else {
            (Color::DarkGray, String::new())
        };
        lines.push(Line::from(vec![
            Span::styled("└─ ", Style::default().fg(Color::DarkGray)),
            Span::styled("referenced", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(
                format!("{}/{} ({}%)", ref_count, hits.len(), rate),
                Style::default().fg(rate_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(note, Style::default().fg(Color::DarkGray)),
        ]));
    }

    // Verdict key so first-time users read the symbols correctly.
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("   key: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "✓",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " next turn cited this hit  ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            "·",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " surfaced but ignored  ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("✗", Style::default().fg(Color::Red)),
        Span::styled(" dropped by scorer", Style::default().fg(Color::DarkGray)),
    ]));

    lines
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}
