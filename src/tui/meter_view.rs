//! Substrate meter — "is Yggdrasil earning its keep?" gauges.
//!
//! Four gauges + a live event tape at the bottom. All numbers come from
//! the events table + agents.metadata.

use chrono::{Duration as CDuration, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap};
use sqlx::PgPool;

pub struct MeterView {
    pub context_pct: u16,       // 0..100
    pub context_tokens: i64,    // estimated from transcript bytes / 10
    pub context_limit: i64,     // configured window (default 200k)
    pub cache_rate: u16,
    pub cache_hits: i64,
    pub cache_total: i64,
    pub referenced_rate: u16,
    pub referenced: i64,
    pub hits_emitted: i64,
    pub prompts_24h: i64,
    pub digests_24h: i64,
    pub nodes_total: i64,
    pub redactions_24h: i64,
    pub last_digest_secs: Option<i64>,
    pub recent_events: Vec<(String, String, String)>, // (ts, kind, detail)
}

impl MeterView {
    pub fn new() -> Self {
        Self {
            context_pct: 0, context_tokens: 0, context_limit: 200_000,
            cache_rate: 0, cache_hits: 0, cache_total: 0,
            referenced_rate: 0, referenced: 0, hits_emitted: 0,
            prompts_24h: 0, digests_24h: 0, nodes_total: 0, redactions_24h: 0,
            last_digest_secs: None,
            recent_events: vec![],
        }
    }

    pub async fn refresh(&mut self, pool: &PgPool, agent_name: &str) -> Result<(), anyhow::Error> {
        let since = Utc::now() - CDuration::hours(24);

        // Cache
        let (hits, calls): (i64, i64) = sqlx::query_as(
            r#"SELECT
                 COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call')
               FROM events WHERE created_at >= $1"#
        ).bind(since).fetch_one(pool).await.unwrap_or((0, 0));
        self.cache_hits = hits;
        self.cache_total = hits + calls;
        self.cache_rate = if self.cache_total > 0 {
            (hits as f64 / self.cache_total as f64 * 100.0) as u16
        } else { 0 };

        // Referenced rate
        let (sim_hits, refd): (i64, i64) = sqlx::query_as(
            r#"SELECT
                 COUNT(*) FILTER (WHERE event_kind::text = 'similarity_hit'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'hit_referenced')
               FROM events WHERE created_at >= $1"#
        ).bind(since).fetch_one(pool).await.unwrap_or((0, 0));
        self.hits_emitted = sim_hits;
        self.referenced = refd;
        self.referenced_rate = if sim_hits > 0 {
            (refd as f64 / sim_hits as f64 * 100.0) as u16
        } else { 0 };

        // Context pressure — compute from the current transcript's bytes.
        // This matches what `ygg bar` / `ygg prime` report (and what the
        // user sees at the top of their session). agents.context_tokens is
        // our own counter and drifts; the transcript file is the source of
        // truth for what Claude Code is actually holding.
        self.context_limit = std::env::var("YGG_CONTEXT_LIMIT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(200_000);
        self.context_tokens = crate::cli::digest::find_latest_transcript()
            .and_then(|p| std::fs::metadata(&p).ok().map(|m| (m.len() / 10) as i64))
            .unwrap_or(0);
        self.context_pct = if self.context_limit > 0 {
            ((self.context_tokens as f64 / self.context_limit as f64) * 100.0).min(999.0) as u16
        } else { 0 };

        // Session counters — prompts, digests, redactions in the last 24h.
        let (p24, d24, r24): (i64, i64, i64) = sqlx::query_as(
            r#"SELECT
                 COUNT(*) FILTER (WHERE event_kind::text = 'node_written' AND payload->>'kind' = 'user_message'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'digest_written'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'redaction_applied')
               FROM events WHERE created_at >= $1"#
        ).bind(since).fetch_one(pool).await.unwrap_or((0, 0, 0));
        self.prompts_24h = p24;
        self.digests_24h = d24;
        self.redactions_24h = r24;

        // Total nodes for this agent — the size of the DAG.
        self.nodes_total = sqlx::query_scalar(
            "SELECT COUNT(*) FROM nodes n JOIN agents a ON a.agent_id = n.agent_id WHERE a.agent_name = $1"
        ).bind(agent_name).fetch_optional(pool).await.ok().flatten().unwrap_or(0);

        // Last digest age
        self.last_digest_secs = sqlx::query_scalar(
            r#"SELECT EXTRACT(EPOCH FROM (now() - n.created_at))::bigint
               FROM nodes n JOIN agents a ON a.agent_id = n.agent_id
               WHERE a.agent_name = $1 AND n.kind = 'digest'
               ORDER BY n.created_at DESC LIMIT 1"#
        ).bind(agent_name).fetch_optional(pool).await.ok().flatten();

        // Event tape (last 10, newest last for bottom scroll feel).
        let rows: Vec<(chrono::DateTime<Utc>, String, serde_json::Value)> = sqlx::query_as(
            "SELECT created_at, event_kind::text, payload FROM events
             ORDER BY created_at DESC LIMIT 10"
        ).fetch_all(pool).await.unwrap_or_default();
        self.recent_events = rows.into_iter().rev().map(|(t, k, p)| {
            let ts = t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
            let detail = short_detail(&k, &p);
            (ts, k, detail)
        }).collect();

        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Split: stats line + gauges + event tape
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(12), Constraint::Min(4)])
            .split(area);

        let stats_area = chunks[0];
        let gauges_area = chunks[1];
        let tape_area = chunks[2];

        // Stats line — raw numbers the gauges can't express.
        let stats = Paragraph::new(Line::from(vec![
            Span::styled(" prompts/24h ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", self.prompts_24h), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled("digests/24h ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", self.digests_24h), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled("nodes ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", self.nodes_total), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled("redacted/24h ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", self.redactions_24h),
                if self.redactions_24h > 0 {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else { Style::default().add_modifier(Modifier::BOLD) }),
        ]))
        .block(Block::default().borders(Borders::ALL).title(" Session stats "));
        frame.render_widget(stats, stats_area);

        let gauges = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .split(gauges_area);

        let ctx_title = format!(" Context pressure — {} / {} tokens ",
            format_tokens(self.context_tokens), format_tokens(self.context_limit));
        let ctx_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(ctx_title))
            .gauge_style(pressure_color(self.context_pct))
            .percent(self.context_pct.min(100));
        frame.render_widget(ctx_gauge, gauges[0]);

        let cache_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL)
                .title(format!(" Cache hit rate — {} / {} embeds in 24h ",
                    self.cache_hits, self.cache_total)))
            .gauge_style(Style::default().fg(Color::Green))
            .percent(self.cache_rate);
        frame.render_widget(cache_gauge, gauges[1]);

        let ref_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL)
                .title(format!(" Referenced rate — {} / {} recalls used in 24h ",
                    self.referenced, self.hits_emitted)))
            .gauge_style(Style::default().fg(Color::Cyan))
            .percent(self.referenced_rate);
        frame.render_widget(ref_gauge, gauges[2]);

        let digest_label = match self.last_digest_secs {
            Some(s) => format!(" Last digest — {} ago ", human_age(s)),
            None => " Last digest — never ".to_string(),
        };
        // Stale: brighter color as time passes. >1h = yellow, >4h = red.
        let digest_pct = match self.last_digest_secs {
            Some(s) if s < 3600 => 100,
            Some(s) if s < 14400 => 60,
            Some(_) => 20,
            None => 0,
        };
        let digest_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(digest_label))
            .gauge_style(Style::default().fg(
                if digest_pct >= 100 { Color::Green }
                else if digest_pct >= 60 { Color::Yellow }
                else { Color::Red }
            ))
            .percent(digest_pct);
        frame.render_widget(digest_gauge, gauges[3]);

        // Tape
        let items: Vec<ListItem> = self.recent_events.iter().map(|(ts, k, d)| {
            ListItem::new(Line::from(vec![
                Span::styled(ts.clone(), Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(format!("{k:<18}"), Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::raw(d.clone()),
            ]))
        }).collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Live events "));
        frame.render_widget(list, tape_area);

        let _ = Paragraph::new("").wrap(Wrap { trim: true });
    }
}

fn format_tokens(n: i64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) }
    else if n >= 1_000 { format!("{:.1}K", n as f64 / 1_000.0) }
    else { format!("{n}") }
}

fn pressure_color(pct: u16) -> Style {
    if pct >= 90 { Style::default().fg(Color::Red) }
    else if pct >= 75 { Style::default().fg(Color::Yellow) }
    else { Style::default().fg(Color::Green) }
}

fn human_age(secs: i64) -> String {
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else if secs < 86400 { format!("{}h", secs / 3600) }
    else { format!("{}d", secs / 86400) }
}

fn short_detail(kind: &str, p: &serde_json::Value) -> String {
    match kind {
        "similarity_hit" => {
            let s = p.get("total_score").or_else(|| p.get("similarity"))
                .and_then(|v| v.as_f64()).unwrap_or(0.0);
            let src = p.get("source_agent").and_then(|v| v.as_str()).unwrap_or("?");
            format!("score={s:.2} from {src}")
        }
        "embedding_call" | "embedding_cache_hit" => {
            let chars = p.get("input_chars").and_then(|v| v.as_u64()).unwrap_or(0);
            let ms = p.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("{chars}c {ms}ms")
        }
        "hit_referenced" => {
            let o = p.get("overlap").and_then(|v| v.as_f64()).unwrap_or(0.0);
            format!("overlap={o:.2}")
        }
        "digest_written" => {
            let t = p.get("turns").and_then(|v| v.as_i64()).unwrap_or(0);
            format!("{t} turns")
        }
        _ => String::new(),
    }
}
