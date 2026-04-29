//! Nerdy stats pane (yggdrasil-178). The deep-dive companion to the
//! status strip's three-line summary: pool internals, table sizes,
//! pgvector index state, and per-hook fire timestamps. Refresh is
//! deliberately *slow* (5 s default) — these aren't liveness signals,
//! they're "why is the system slow / how big has the corpus grown."

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};
use sqlx::PgPool;

use super::app::{OpsStats, cost_hidden, format_tokens_per_min};

use super::ctx_usage::{
    HARD_DANGER, SOFT_DEGRADATION, SOFT_HARD_WARN, agent_usage_breakdown, ctx_color,
    humanize_tokens,
};
use crate::models::agent::AgentRepo;

#[derive(Debug, Default, Clone)]
pub struct NerdyStats {
    pub pool: PoolStats,
    pub tables: Vec<TableStat>,
    pub pgvector: PgvectorStats,
    pub hook_kinds: Vec<HookFire>,
    pub tokens: TokenStats,
    pub loaded: bool,
    pub last_status: String,
}

/// Aggregated context-window usage across all live (non-archived)
/// agents. Numbers come from the latest `usage` block in each agent's
/// CC transcript JSONL (cache_read + cache_creation + input + output).
#[derive(Debug, Default, Clone)]
pub struct TokenStats {
    /// How many agents had a parseable usage block.
    pub sessions: usize,
    /// Sum of all latest-turn totals (cache_read + cache_creation +
    /// input + output) across sessions.
    pub fleet_total: i64,
    /// Component breakdowns aggregated across sessions.
    pub cache_read: i64,
    pub cache_creation: i64,
    pub input: i64,
    pub output: i64,
    /// Largest single session: (agent_name, tokens, hard_cap).
    pub largest: Option<(String, i64, i64)>,
    /// Sessions past the soft-degradation knee (≥ 200K).
    pub past_soft: usize,
    /// Sessions past the orange knee (≥ 300K).
    pub past_warn: usize,
    /// Sessions ≥ 80% of their detected hard cap.
    pub past_hard: usize,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PoolStats {
    pub used: u32,
    pub idle: u32,
    pub max: u32,
}

#[derive(Debug, Clone)]
pub struct TableStat {
    pub name: String,
    pub rows: i64,
    pub bytes: i64,
    /// Ratio of dead tuples to live tuples — high values indicate
    /// autovacuum is falling behind.
    pub dead_ratio: f64,
}

#[derive(Debug, Default, Clone)]
pub struct PgvectorStats {
    pub installed: bool,
    pub version: Option<String>,
    /// (table, index_name, dimensions) per pgvector index.
    pub indexes: Vec<(String, String, Option<i32>)>,
}

#[derive(Debug, Clone)]
pub struct HookFire {
    pub kind: String,
    pub last_fired: Option<DateTime<Utc>>,
    pub count_24h: i64,
}

pub struct NerdyView {
    pub stats: NerdyStats,
    pub ops: OpsStats,
    /// Events-per-hour sparkline, last 24h (24 buckets, oldest→newest).
    pub events_hourly: Vec<u64>,
    /// Per-agent context bars: (name, tokens, hard_cap) sorted desc by tokens.
    pub agent_bars: Vec<(String, i64, i64)>,
}

impl Default for NerdyView {
    fn default() -> Self {
        Self::new()
    }
}

impl NerdyView {
    pub fn new() -> Self {
        Self {
            stats: NerdyStats::default(),
            ops: OpsStats::default(),
            events_hourly: vec![0; 24],
            agent_bars: vec![],
        }
    }

    pub fn update_ops(&mut self, ops: &OpsStats) {
        self.ops = ops.clone();
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        self.stats = collect(pool).await;

        // Events-per-hour sparkline — 24 buckets.
        let rows: Vec<(i32, i64)> = sqlx::query_as(
            "SELECT (24 - FLOOR(EXTRACT(EPOCH FROM (now() - created_at)) / 3600))::int,
                    COUNT(*)
             FROM events
             WHERE created_at >= now() - interval '24 hours'
             GROUP BY 1 ORDER BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        let mut series = vec![0u64; 24];
        for (b, n) in rows {
            let idx = (b - 1).clamp(0, 23) as usize;
            series[idx] = n as u64;
        }
        self.events_hourly = series;

        // Per-agent context bars from live transcripts.
        let mut bars: Vec<(String, i64, i64)> = Vec::new();
        if let Ok(agents) = AgentRepo::new(pool, crate::db::user_id()).list().await {
            for a in &agents {
                if let Some(b) = agent_usage_breakdown(&a.agent_name) {
                    bars.push((a.agent_name.clone(), b.total(), b.hard_cap));
                }
            }
        }
        bars.sort_by(|a, b| b.1.cmp(&a.1));
        bars.truncate(15);
        self.agent_bars = bars;

        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.stats.loaded {
            let p = Paragraph::new(" loading nerdy stats… ")
                .block(Block::default().borders(Borders::ALL).title(" Nerdy "));
            frame.render_widget(p, area);
            return;
        }

        // Split: left text panel (65%) | right sparkline + context bars (35%)
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area);

        let mut lines: Vec<Line> = Vec::with_capacity(80);

        // ── Live Signals ─────────────────────────────────────────
        lines.push(section_header("live signals"));
        let o = &self.ops;
        lines.push(kv("throughput", &format_tokens_per_min(o.tokens_per_min)));
        if !cost_hidden() {
            lines.push(kv("cost today", &format!("${:.2}", o.cost_today_usd)));
            lines.push(kv("tokens today", &humanize_tokens(o.tokens_today)));
        }
        lines.push(kv("events/min", &format!("{}", o.events_per_min)));
        let pool_pct = if o.pool_max > 0 {
            format!(
                "{}/{} ({:.0}%)",
                o.pool_used,
                o.pool_max,
                o.pool_used as f64 / o.pool_max as f64 * 100.0
            )
        } else {
            "?".into()
        };
        lines.push(kv("pool saturation", &pool_pct));
        lines.push(kv("db latency", &format!("{}ms", o.db_ms)));
        let health = |ok: bool| -> Span<'static> {
            if ok {
                Span::styled("OK", Style::default().fg(Color::Green))
            } else {
                Span::styled(
                    "DOWN",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            }
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("    {:<26}", "ollama"),
                Style::default().fg(Color::Gray),
            ),
            health(o.ollama_ok),
            Span::raw("    "),
            Span::styled("pgvector  ", Style::default().fg(Color::Gray)),
            health(o.pgvector_ok),
        ]));
        lines.push(Line::from(""));

        // ── Tokens ───────────────────────────────────────────────
        let t = &self.stats.tokens;
        lines.push(section_header(&format!(
            "tokens (live across {} session{})",
            t.sessions,
            if t.sessions == 1 { "" } else { "s" }
        )));
        if t.sessions == 0 {
            lines.push(dim("(no parseable transcripts)"));
        } else {
            lines.push(kv("fleet total", &humanize_tokens(t.fleet_total)));
            let total = t.fleet_total.max(1);
            lines.push(kv(
                "cache_read",
                &format!(
                    "{} ({:.0}%)",
                    humanize_tokens(t.cache_read),
                    100.0 * t.cache_read as f64 / total as f64
                ),
            ));
            lines.push(kv(
                "cache_creation",
                &format!(
                    "{} ({:.0}%)",
                    humanize_tokens(t.cache_creation),
                    100.0 * t.cache_creation as f64 / total as f64
                ),
            ));
            lines.push(kv(
                "input + output",
                &format!(
                    "{} + {} ({:.0}% + {:.0}%)",
                    humanize_tokens(t.input),
                    humanize_tokens(t.output),
                    100.0 * t.input as f64 / total as f64,
                    100.0 * t.output as f64 / total as f64,
                ),
            ));
            if let Some((name, tokens, _hard)) = &t.largest {
                let color = ctx_color(*tokens);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("    {:<26}", "largest"),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(
                        format!("{} {}", name, humanize_tokens(*tokens)),
                        Style::default().fg(color),
                    ),
                ]));
            }
            let thresholds = format!(
                "{} soft / {} warn / {} danger",
                t.past_soft, t.past_warn, t.past_hard
            );
            lines.push(kv("past thresholds", &thresholds));
        }
        lines.push(Line::from(""));

        // ── Pool ─────────────────────────────────────────────────
        lines.push(section_header("pool"));
        let p = &self.stats.pool;
        lines.push(kv(
            "used / idle / max",
            &format!("{} / {} / {}", p.used, p.idle, p.max),
        ));
        lines.push(Line::from(""));

        // ── Tables ───────────────────────────────────────────────
        lines.push(section_header("tables"));
        if self.stats.tables.is_empty() {
            lines.push(dim("(no rows)"));
        } else {
            for t in &self.stats.tables {
                let warn = if t.dead_ratio >= 0.30 { " ⚠" } else { "" };
                lines.push(kv(
                    &t.name,
                    &format!(
                        "{} rows · {} · dead {:.0}%{warn}",
                        humanize_count(t.rows),
                        humanize_bytes(t.bytes),
                        t.dead_ratio * 100.0
                    ),
                ));
            }
        }
        lines.push(Line::from(""));

        // ── pgvector ─────────────────────────────────────────────
        lines.push(section_header("pgvector"));
        let v = &self.stats.pgvector;
        if v.installed {
            lines.push(kv("extension", v.version.as_deref().unwrap_or("?")));
            if v.indexes.is_empty() {
                lines.push(dim("  no vector indexes"));
            } else {
                for (table, name, dims) in &v.indexes {
                    let dims_str = dims.map(|d| format!("{d}d")).unwrap_or_else(|| "?".into());
                    lines.push(kv(&format!("  {table}.{name}"), &dims_str));
                }
            }
        } else {
            lines.push(Line::from(Span::styled(
                "  ✗ pgvector NOT installed",
                Style::default().fg(Color::Red),
            )));
        }
        lines.push(Line::from(""));

        // ── Hooks ────────────────────────────────────────────────
        lines.push(section_header("hooks (last fire / 24h count)"));
        if self.stats.hook_kinds.is_empty() {
            lines.push(dim("(no hook_fired events)"));
        } else {
            let now = Utc::now();
            for h in &self.stats.hook_kinds {
                let age = h
                    .last_fired
                    .map(|t| humanize_age(now - t))
                    .unwrap_or_else(|| "never".into());
                lines.push(kv(&h.kind, &format!("{age} · {} fires", h.count_24h)));
            }
        }

        let para = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Nerdy · live / tokens / pool / tables / pgvector / hooks "),
        );
        frame.render_widget(para, cols[0]);

        // ── Right panel: sparkline + context bars ────────────────
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6), // events sparkline
                Constraint::Min(4),    // per-agent context bars
            ])
            .split(cols[1]);

        // Events/hour sparkline
        let max = *self.events_hourly.iter().max().unwrap_or(&1);
        let spark = Sparkline::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Events/hour 24h · peak {} ", max)),
            )
            .data(&self.events_hourly)
            .max(max.max(1))
            .style(Style::default().fg(Color::Magenta));
        frame.render_widget(spark, right[0]);

        // Per-agent context bars — horizontal bars sized relative to
        // the largest session's hard_cap.
        let bar_width = right[1].width.saturating_sub(2) as usize; // inside borders
        let max_cap = self
            .agent_bars
            .iter()
            .map(|(_, _, cap)| *cap)
            .max()
            .unwrap_or(300_000)
            .max(1);
        let mut bar_lines: Vec<Line> = Vec::new();
        for (name, tokens, cap) in &self.agent_bars {
            let label_width = 16.min(bar_width / 3);
            let bar_space = bar_width.saturating_sub(label_width + 10);
            let filled = if *cap > 0 {
                ((*tokens as f64 / *cap as f64) * bar_space as f64).round() as usize
            } else {
                ((*tokens as f64 / max_cap as f64) * bar_space as f64).round() as usize
            }
            .min(bar_space);
            let color = ctx_color(*tokens);
            let bar: String = "█".repeat(filled) + &"░".repeat(bar_space.saturating_sub(filled));
            let truncated_name = if name.len() > label_width {
                &name[..label_width]
            } else {
                name
            };
            bar_lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<width$}", truncated_name, width = label_width),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(bar, Style::default().fg(color)),
                Span::styled(
                    format!(" {}", humanize_tokens(*tokens)),
                    Style::default().fg(color),
                ),
            ]));
        }
        if bar_lines.is_empty() {
            bar_lines.push(Line::from(Span::styled(
                " (no live transcripts)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        let bars_widget = Paragraph::new(bar_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Context per agent "),
        );
        frame.render_widget(bars_widget, right[1]);
    }
}

async fn collect(pool: &PgPool) -> NerdyStats {
    let mut out = NerdyStats {
        loaded: true,
        ..Default::default()
    };
    out.pool = PoolStats {
        used: pool.size().saturating_sub(pool.num_idle() as u32),
        idle: pool.num_idle() as u32,
        max: pool.options().get_max_connections(),
    };

    // Tokens — walk every registered agent's latest CC transcript and
    // aggregate the most-recent `usage` block. Disk-bound but cheap
    // (200KB tail per file) and only fires at the nerdy refresh
    // cadence (5s).
    if let Ok(agents) = AgentRepo::new(pool, crate::db::user_id()).list().await {
        let mut ts = TokenStats::default();
        for a in &agents {
            let Some(b) = agent_usage_breakdown(&a.agent_name) else {
                continue;
            };
            ts.sessions += 1;
            ts.cache_read += b.cache_read;
            ts.cache_creation += b.cache_creation;
            ts.input += b.input;
            ts.output += b.output;
            let total = b.total();
            ts.fleet_total += total;
            if total >= SOFT_DEGRADATION {
                ts.past_soft += 1;
            }
            if total >= SOFT_HARD_WARN {
                ts.past_warn += 1;
            }
            if total >= HARD_DANGER {
                ts.past_hard += 1;
            }
            match &ts.largest {
                None => ts.largest = Some((a.agent_name.clone(), total, b.hard_cap)),
                Some((_, prev, _)) if total > *prev => {
                    ts.largest = Some((a.agent_name.clone(), total, b.hard_cap))
                }
                _ => {}
            }
        }
        out.tokens = ts;
    }

    // Table sizes — pg_total_relation_size() per known orchestrator
    // table; deduplicated by joining pg_stat_user_tables for dead-tuple
    // ratio. Limited to the tables we care about so a sprawling DB
    // doesn't pollute the view.
    let table_query = r#"
        WITH wanted AS (
          SELECT unnest(ARRAY[
            'tasks', 'task_runs', 'nodes', 'events',
            'agent_stats', 'learnings', 'memories',
            'task_deps', 'locks'
          ]) AS name
        )
        SELECT
          w.name,
          COALESCE(s.n_live_tup, 0)::bigint  AS rows,
          COALESCE(pg_total_relation_size(w.name::regclass), 0)::bigint AS bytes,
          CASE WHEN COALESCE(s.n_live_tup, 0) > 0
               THEN (COALESCE(s.n_dead_tup, 0)::float / s.n_live_tup::float)
               ELSE 0.0
          END AS dead_ratio
        FROM wanted w
        LEFT JOIN pg_stat_user_tables s
          ON s.relname = w.name AND s.schemaname = 'public'
        WHERE EXISTS (
          SELECT 1 FROM pg_class c WHERE c.relname = w.name
        )
        ORDER BY bytes DESC
    "#;
    if let Ok(rows) = sqlx::query_as::<_, (String, i64, i64, f64)>(table_query)
        .fetch_all(pool)
        .await
    {
        out.tables = rows
            .into_iter()
            .map(|(name, rows, bytes, dead_ratio)| TableStat {
                name,
                rows,
                bytes,
                dead_ratio,
            })
            .collect();
    }

    // pgvector — extension version + index list.
    if let Ok(version) = sqlx::query_scalar::<_, String>(
        "SELECT extversion FROM pg_extension WHERE extname = 'vector'",
    )
    .fetch_optional(pool)
    .await
    {
        if let Some(v) = version {
            out.pgvector.installed = true;
            out.pgvector.version = Some(v);
            let idx_q = r#"
                SELECT
                  c2.relname AS table_name,
                  c.relname  AS index_name,
                  CASE WHEN a.atttypmod > 0 THEN a.atttypmod ELSE NULL END AS dimensions
                FROM pg_class c
                JOIN pg_index i      ON i.indexrelid = c.oid
                JOIN pg_class c2     ON c2.oid = i.indrelid
                JOIN pg_am am        ON am.oid = c.relam
                JOIN pg_attribute a  ON a.attrelid = c2.oid AND a.attnum = i.indkey[0]
                WHERE am.amname IN ('hnsw', 'ivfflat')
                ORDER BY c2.relname, c.relname
            "#;
            if let Ok(rows) = sqlx::query_as::<_, (String, String, Option<i32>)>(idx_q)
                .fetch_all(pool)
                .await
            {
                out.pgvector.indexes = rows;
            }
        }
    }

    // Hooks — last fire + 24h count per hook_fired payload kind.
    let hook_q = r#"
        SELECT
          payload->>'hook' AS kind,
          MAX(created_at)  AS last_fired,
          COUNT(*) FILTER (WHERE created_at > now() - interval '24 hours')::bigint AS count_24h
        FROM events
        WHERE event_kind = 'hook_fired'
          AND payload ? 'hook'
        GROUP BY payload->>'hook'
        ORDER BY MAX(created_at) DESC NULLS LAST
        LIMIT 20
    "#;
    if let Ok(rows) = sqlx::query_as::<_, (Option<String>, Option<DateTime<Utc>>, i64)>(hook_q)
        .fetch_all(pool)
        .await
    {
        out.hook_kinds = rows
            .into_iter()
            .filter_map(|(kind, last, count)| {
                kind.map(|k| HookFire {
                    kind: k,
                    last_fired: last,
                    count_24h: count,
                })
            })
            .collect();
    }

    out
}

fn section_header(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {label}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("    {key:<26}"), Style::default().fg(Color::Gray)),
        Span::styled(value.to_string(), Style::default().fg(Color::White)),
    ])
}

fn dim(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("    {s}"),
        Style::default().fg(Color::DarkGray),
    ))
}

/// Humanise a row count: 1234 → "1.2k", 1_234_567 → "1.2M".
pub fn humanize_count(n: i64) -> String {
    let abs = n.unsigned_abs() as f64;
    if abs >= 1_000_000.0 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Humanise a byte count.
pub fn humanize_bytes(n: i64) -> String {
    let abs = n.unsigned_abs() as f64;
    if abs >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if abs >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else if abs >= 1024.0 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// Humanise a chrono Duration as "5s", "12m", "3h", "2d".
pub fn humanize_age(d: chrono::Duration) -> String {
    let s = d.num_seconds();
    if s < 0 {
        return "future".into();
    }
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}
