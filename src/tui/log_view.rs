//! Live log viewer — events table tail inside the TUI. `f` cycles a
//! kind filter, arrows scroll, `r` forces refresh.

use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use sqlx::PgPool;

const FILTERS: &[&str] = &[
    "all",
    "similarity_hit",
    "embedding_call",
    "embedding_cache_hit",
    "scoring_decision",
    "hit_referenced",
    "digest_written",
    "node_written",
    "redaction_applied",
    "task_created",
    "task_status_changed",
    "lock_acquired",
];

pub struct LogView {
    pub events: Vec<(DateTime<Utc>, String, String, serde_json::Value)>,
    pub filter_idx: usize,
    pub state: ListState,
    pub limit: i64,
}

impl LogView {
    pub fn new() -> Self {
        let mut st = ListState::default();
        st.select(Some(0));
        Self { events: vec![], filter_idx: 0, state: st, limit: 200 }
    }

    pub fn filter(&self) -> &'static str { FILTERS[self.filter_idx] }

    pub fn cycle_filter(&mut self) {
        self.filter_idx = (self.filter_idx + 1) % FILTERS.len();
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let rows = if self.filter() == "all" {
            sqlx::query_as::<_, (DateTime<Utc>, String, String, serde_json::Value)>(
                "SELECT created_at, event_kind::text, agent_name, payload
                 FROM events ORDER BY created_at DESC LIMIT $1"
            ).bind(self.limit).fetch_all(pool).await.unwrap_or_default()
        } else {
            sqlx::query_as(
                "SELECT created_at, event_kind::text, agent_name, payload
                 FROM events WHERE event_kind::text = $1
                 ORDER BY created_at DESC LIMIT $2"
            ).bind(self.filter()).bind(self.limit).fetch_all(pool).await.unwrap_or_default()
        };
        // Newest last for bottom-reading feel
        self.events = rows.into_iter().rev().collect();
        if !self.events.is_empty() && self.state.selected().is_none() {
            self.state.select(Some(self.events.len() - 1));
        }
        Ok(())
    }

    pub fn scroll_up(&mut self) {
        if self.events.is_empty() { return; }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(if i == 0 { 0 } else { i - 1 }));
    }

    pub fn scroll_down(&mut self) {
        if self.events.is_empty() { return; }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1).min(self.events.len() - 1)));
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self.events.iter().map(|(ts, kind, agent, p)| {
            let ts_str = ts.with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
            let (color, glyph) = kind_style(kind);
            let detail = short_detail(kind, p);
            ListItem::new(Line::from(vec![
                Span::styled(ts_str, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(format!("{glyph}{kind:<20}"), Style::default().fg(color)),
                Span::styled(format!(" {agent:<16}"), Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::raw(detail),
            ]))
        }).collect();

        let title = format!(" Logs — filter: [{}]  (f: cycle)  {} events ",
            self.filter(), self.events.len());
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

fn kind_style(kind: &str) -> (Color, &'static str) {
    match kind {
        "similarity_hit" => (Color::Cyan, "≈"),
        "embedding_call" => (Color::Blue, "⚡"),
        "embedding_cache_hit" => (Color::Green, "⚡"),
        "scoring_decision" => (Color::DarkGray, "·"),
        "classifier_decision" => (Color::Magenta, "⚖"),
        "hit_referenced" => (Color::Green, "✓"),
        "digest_written" => (Color::Yellow, "◈"),
        "node_written" => (Color::Green, "●"),
        "redaction_applied" => (Color::Red, "✂"),
        "task_created" => (Color::Green, "✚"),
        "task_status_changed" => (Color::Yellow, "◆"),
        "lock_acquired" => (Color::Yellow, "⚿"),
        "lock_released" => (Color::DarkGray, "○"),
        "remembered" => (Color::Cyan, "♦"),
        _ => (Color::White, "·"),
    }
}

fn short_detail(kind: &str, p: &serde_json::Value) -> String {
    match kind {
        "similarity_hit" => {
            let s = p.get("total_score").or_else(|| p.get("similarity"))
                .and_then(|v| v.as_f64()).unwrap_or(0.0);
            let snip = p.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            format!("score={s:.2} · {}", truncate(snip, 50))
        }
        "embedding_call" | "embedding_cache_hit" => {
            let chars = p.get("input_chars").and_then(|v| v.as_u64()).unwrap_or(0);
            let ms = p.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            let purpose = p.get("purpose").and_then(|v| v.as_str()).unwrap_or("");
            format!("{chars}c {ms}ms {purpose}")
        }
        "digest_written" => {
            let t = p.get("turns").and_then(|v| v.as_i64()).unwrap_or(0);
            let c = p.get("corrections").and_then(|v| v.as_i64()).unwrap_or(0);
            let method = p.get("method").and_then(|v| v.as_str()).unwrap_or("");
            format!("{t} turns · {c} corr · {method}")
        }
        "node_written" => {
            let k = p.get("kind").and_then(|v| v.as_str()).unwrap_or("node");
            let snip = p.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            format!("[{k}] {}", truncate(snip, 50))
        }
        "task_created" => {
            let r = p.get("ref").and_then(|v| v.as_str()).unwrap_or("?");
            let t = p.get("title").and_then(|v| v.as_str()).unwrap_or("");
            format!("{r} — {}", truncate(t, 50))
        }
        "task_status_changed" => {
            let r = p.get("ref").and_then(|v| v.as_str()).unwrap_or("?");
            let to = p.get("to").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{r} → {to}")
        }
        "hit_referenced" => {
            let o = p.get("overlap").and_then(|v| v.as_f64()).unwrap_or(0.0);
            format!("overlap={o:.2}")
        }
        "redaction_applied" => {
            let total = p.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
            let kinds = p.get("kinds").and_then(|v| v.as_object())
                .map(|o| o.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            format!("{total} secret(s) · {kinds}")
        }
        "lock_acquired" | "lock_released" => {
            p.get("resource").and_then(|v| v.as_str()).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { return s; }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    &s[..end]
}
