//! Eval pane — live retrieval effectiveness metrics.
//! Mirrors `ygg eval` CLI but auto-refreshes so you can watch the numbers
//! move while exercising the system.

use chrono::{Duration, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use sqlx::PgPool;

pub struct EvalView {
    pub window_hours: i64,
    pub prompts: i64,
    pub hits: i64,
    pub avg_per_prompt: f64,
    pub referenced: i64,

    pub scoring_kept: i64,
    pub scoring_dropped: i64,
    pub drop_reasons: Vec<(String, i64)>,

    pub cls_kept: i64,
    pub cls_dropped: i64,
    pub cls_bypassed: i64,

    pub embed_calls: i64,
    pub cache_hits: i64,
    /// Mean latency (ms) of actual embedding calls in the window — used to
    /// multiply cache_hits into wall-time savings.
    pub avg_embed_ms: f64,

    pub nodes_written: i64,
    pub digests_written: i64,
    pub locks_acquired: i64,
    pub locks_released: i64,
    pub redactions: i64,

    /// Lifetime counters — cheap to aggregate across the whole events table.
    pub lifetime_cache_hits: i64,
    pub lifetime_referenced: i64,
    pub lifetime_redactions: i64,
    pub lifetime_digests: i64,
}

impl EvalView {
    pub fn new() -> Self {
        Self {
            window_hours: 24,
            prompts: 0, hits: 0, avg_per_prompt: 0.0, referenced: 0,
            scoring_kept: 0, scoring_dropped: 0, drop_reasons: vec![],
            cls_kept: 0, cls_dropped: 0, cls_bypassed: 0,
            embed_calls: 0, cache_hits: 0, avg_embed_ms: 0.0,
            nodes_written: 0, digests_written: 0,
            locks_acquired: 0, locks_released: 0, redactions: 0,
            lifetime_cache_hits: 0, lifetime_referenced: 0,
            lifetime_redactions: 0, lifetime_digests: 0,
        }
    }

    pub fn cycle_window(&mut self) {
        self.window_hours = match self.window_hours {
            1 => 6,
            6 => 24,
            24 => 168, // 1 week
            _ => 1,
        };
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let since = Utc::now() - Duration::hours(self.window_hours);

        self.prompts = count_where(pool, since,
            "event_kind::text = 'node_written' AND payload->>'snippet' IS NOT NULL").await;
        self.hits = count_where(pool, since,
            "event_kind::text = 'similarity_hit'").await;
        self.avg_per_prompt = if self.prompts > 0 {
            self.hits as f64 / self.prompts as f64
        } else { 0.0 };
        self.referenced = count_where(pool, since,
            "event_kind::text = 'hit_referenced'").await;

        self.scoring_kept = count_where(pool, since,
            "event_kind::text = 'scoring_decision' AND (payload->>'kept')::bool = true").await;
        self.scoring_dropped = count_where(pool, since,
            "event_kind::text = 'scoring_decision' AND (payload->>'kept')::bool = false").await;

        self.drop_reasons = sqlx::query_as::<_, (String, i64)>(
            "SELECT payload->>'drop_reason' AS r, COUNT(*)::bigint AS n
             FROM events
             WHERE event_kind::text = 'scoring_decision'
               AND (payload->>'kept')::bool = false
               AND payload->>'drop_reason' IS NOT NULL
               AND created_at >= $1
             GROUP BY r ORDER BY n DESC LIMIT 5"
        ).bind(since).fetch_all(pool).await.unwrap_or_default();

        self.cls_kept = count_where(pool, since,
            "event_kind::text = 'classifier_decision' AND (payload->>'kept')::bool = true AND (payload->>'bypassed')::bool = false").await;
        self.cls_dropped = count_where(pool, since,
            "event_kind::text = 'classifier_decision' AND (payload->>'kept')::bool = false").await;
        self.cls_bypassed = count_where(pool, since,
            "event_kind::text = 'classifier_decision' AND (payload->>'bypassed')::bool = true").await;

        self.embed_calls = count_where(pool, since, "event_kind::text = 'embedding_call'").await;
        self.cache_hits = count_where(pool, since, "event_kind::text = 'embedding_cache_hit'").await;
        // Mean latency of actual embedding calls in the window — drives the
        // wall-time-saved estimate for cache hits.
        self.avg_embed_ms = sqlx::query_scalar::<_, Option<f64>>(
            "SELECT AVG((payload->>'latency_ms')::float)
               FROM events
              WHERE event_kind::text = 'embedding_call'
                AND payload->>'latency_ms' IS NOT NULL
                AND created_at >= $1"
        ).bind(since).fetch_one(pool).await.ok().flatten().unwrap_or(0.0);

        self.nodes_written = count_where(pool, since, "event_kind::text = 'node_written'").await;
        self.digests_written = count_where(pool, since, "event_kind::text = 'digest_written'").await;
        self.locks_acquired = count_where(pool, since, "event_kind::text = 'lock_acquired'").await;
        self.locks_released = count_where(pool, since, "event_kind::text = 'lock_released'").await;
        self.redactions = count_where(pool, since, "event_kind::text = 'redaction_applied'").await;

        // Lifetime totals — every install persists events, so these are the
        // "since install" numbers without needing a separate counter table.
        self.lifetime_cache_hits = lifetime_count(pool, "embedding_cache_hit").await;
        self.lifetime_referenced = lifetime_count(pool, "hit_referenced").await;
        self.lifetime_redactions = lifetime_count(pool, "redaction_applied").await;
        self.lifetime_digests    = lifetime_count(pool, "digest_written").await;
        Ok(())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let window_label = match self.window_hours {
            1 => "1h".to_string(),
            6 => "6h".to_string(),
            24 => "24h".to_string(),
            168 => "7d".to_string(),
            n => format!("{n}h"),
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),   // retrieval
                Constraint::Length(7),   // classifier + cache
                Constraint::Length(9),   // savings (window + lifetime)
                Constraint::Length(5),   // activity
                Constraint::Min(0),      // drop reasons
            ]).split(area);

        let ref_rate = if self.hits > 0 {
            (self.referenced as f64 / self.hits as f64 * 100.0) as i64
        } else { 0 };
        let ref_color = if ref_rate >= 40 { Color::Green }
                       else if ref_rate >= 20 { Color::Yellow }
                       else { Color::DarkGray };

        let retrieval = vec![
            Line::from(Span::styled("  Retrieval",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
            Line::from(""),
            line("user prompts",            &format!("{}", self.prompts)),
            line("similarity hits",         &format!("{}", self.hits)),
            line("avg hits per prompt",     &format!("{:.1}", self.avg_per_prompt)),
            Line::from(vec![
                Span::raw("    "),
                Span::styled("referenced by next turn      ",
                    Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}/{} ({}%)", self.referenced, self.hits, ref_rate),
                    Style::default().fg(ref_color).add_modifier(Modifier::BOLD)),
            ]),
        ];
        let para = Paragraph::new(retrieval).block(
            Block::default().borders(Borders::ALL)
                .title(format!(" Eval — window {window_label}  (w=cycle) "))
        );
        frame.render_widget(para, chunks[0]);

        // Classifier + cache panel
        let cache_total = self.embed_calls + self.cache_hits;
        let cache_rate = if cache_total > 0 {
            (self.cache_hits as f64 / cache_total as f64 * 100.0) as i64
        } else { 0 };
        let cache_color = if cache_rate >= 60 { Color::Green }
                         else if cache_rate >= 30 { Color::Yellow }
                         else { Color::DarkGray };
        let cls_total = self.cls_kept + self.cls_dropped + self.cls_bypassed;
        let cls_line: Line = if cls_total == 0 {
            Line::from(vec![
                Span::raw("    "),
                Span::styled("classifier                 ", Style::default().fg(Color::DarkGray)),
                Span::styled("disabled", Style::default().fg(Color::DarkGray)),
            ])
        } else {
            Line::from(vec![
                Span::raw("    "),
                Span::styled("classifier kept/drop/bypass ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{} / {} / {}", self.cls_kept, self.cls_dropped, self.cls_bypassed)),
            ])
        };
        let cls_cache = vec![
            Line::from(Span::styled("  Classifier & cache",
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))),
            Line::from(""),
            line("scoring kept / dropped", &format!("{} / {}", self.scoring_kept, self.scoring_dropped)),
            cls_line,
            Line::from(vec![
                Span::raw("    "),
                Span::styled("embedding cache hit rate    ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}% ({}/{})", cache_rate, self.cache_hits, cache_total),
                    Style::default().fg(cache_color).add_modifier(Modifier::BOLD)),
            ]),
        ];
        let para = Paragraph::new(cls_cache).block(
            Block::default().borders(Borders::ALL).title(" Retrieval pipeline ")
        );
        frame.render_widget(para, chunks[1]);

        // ── Savings section ─────────────────────────────────────────────
        // Cache hits × mean embedding latency gives us wall-time saved on
        // Ollama round-trips. Referenced hits are the "context recall worked"
        // number. Redactions are the "secret blocked" count (security, not
        // cost). Each line pairs a plain number with a human-readable
        // interpretation so the user sees WHAT they're getting.
        let saved_ms = self.cache_hits as f64 * self.avg_embed_ms;
        let saved_sec = saved_ms / 1000.0;
        let saved_label = if saved_sec >= 60.0 {
            format!("~{:.1} min wall-time", saved_sec / 60.0)
        } else if saved_sec > 0.0 {
            format!("~{saved_sec:.1} sec wall-time")
        } else {
            "(no cache hits yet)".to_string()
        };

        let ref_rate_life = if self.lifetime_cache_hits > 0 || self.lifetime_referenced > 0 {
            format!(
                "{} cached calls  ·  {} hits referenced  ·  {} secrets blocked  ·  {} digests preserved",
                self.lifetime_cache_hits, self.lifetime_referenced,
                self.lifetime_redactions, self.lifetime_digests,
            )
        } else {
            "(no lifetime data yet)".to_string()
        };

        let savings = vec![
            Line::from(Span::styled("  Estimated savings (this window)",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))),
            Line::from(""),
            line("ollama calls avoided",
                &format!("{}  ({saved_label})", self.cache_hits)),
            line("context recall worked",
                &format!("{} hits used by next turn (would have re-explained)", self.referenced)),
            line("secrets blocked at write",
                &format!("{}", self.redactions)),
            Line::from(""),
            Line::from(Span::styled("  Since install",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
            Line::from(vec![
                Span::raw("    "),
                Span::styled(ref_rate_life, Style::default().fg(Color::White)),
            ]),
        ];
        let para = Paragraph::new(savings).block(
            Block::default().borders(Borders::ALL).title(" What are you getting? ")
        );
        frame.render_widget(para, chunks[2]);

        let activity = vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("nodes {} / digests {} / locks {}/{} / redactions {}",
                    self.nodes_written, self.digests_written,
                    self.locks_acquired, self.locks_released, self.redactions),
                    Style::default().fg(Color::Gray)),
            ]),
        ];
        let para = Paragraph::new(activity).block(
            Block::default().borders(Borders::ALL).title(" Activity ")
        );
        frame.render_widget(para, chunks[3]);

        // Drop reasons table
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled("  Top drop reasons",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
            Line::from(""),
        ];
        if self.drop_reasons.is_empty() {
            lines.push(Line::from(Span::styled(
                "    (no drops in this window)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (r, n) in &self.drop_reasons {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("{:<28}", r), Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{n}"),
                        Style::default().add_modifier(Modifier::BOLD)),
                ]));
            }
        }
        let para = Paragraph::new(lines).block(
            Block::default().borders(Borders::ALL).title(" Why did scoring drop things? ")
        );
        frame.render_widget(para, chunks[4]);
    }
}

async fn count_where(pool: &PgPool, since: chrono::DateTime<Utc>, predicate: &str) -> i64 {
    let sql = format!(
        "SELECT COUNT(*)::bigint FROM events WHERE {predicate} AND created_at >= $1"
    );
    sqlx::query_scalar::<_, i64>(&sql).bind(since).fetch_one(pool).await.unwrap_or(0)
}

async fn lifetime_count(pool: &PgPool, kind: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM events WHERE event_kind::text = $1"
    ).bind(kind).fetch_one(pool).await.unwrap_or(0)
}

fn line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("    "),
        Span::styled(format!("{:<28}", label), Style::default().fg(Color::DarkGray)),
        Span::styled(value.to_string(), Style::default().add_modifier(Modifier::BOLD)),
    ])
}
