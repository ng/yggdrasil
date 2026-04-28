//! Nerdy stats pane (yggdrasil-178). The deep-dive companion to the
//! status strip's three-line summary: pool internals, table sizes,
//! pgvector index state, and per-hook fire timestamps. Refresh is
//! deliberately *slow* (5 s default) — these aren't liveness signals,
//! they're "why is the system slow / how big has the corpus grown."

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use sqlx::PgPool;

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
        }
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        self.stats = collect(pool).await;
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.stats.loaded {
            let p = Paragraph::new(" loading nerdy stats… ")
                .block(Block::default().borders(Borders::ALL).title(" Nerdy "));
            frame.render_widget(p, area);
            return;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(64);

        // ── Tokens ───────────────────────────────────────────────
        // Live context-window roll-up across all agents — flags how
        // many sessions are sitting past the soft-degradation knee
        // even when none individually trip the hard-cap alert.
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
                "input",
                &format!(
                    "{} ({:.0}%)",
                    humanize_tokens(t.input),
                    100.0 * t.input as f64 / total as f64
                ),
            ));
            lines.push(kv(
                "output",
                &format!(
                    "{} ({:.0}%)",
                    humanize_tokens(t.output),
                    100.0 * t.output as f64 / total as f64
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
            lines.push(kv(
                &format!("past soft (≥{})", humanize_tokens(SOFT_DEGRADATION)),
                &format!(
                    "{} session{}",
                    t.past_soft,
                    if t.past_soft == 1 { "" } else { "s" }
                ),
            ));
            lines.push(kv(
                &format!("past warn (≥{})", humanize_tokens(SOFT_HARD_WARN)),
                &format!(
                    "{} session{}",
                    t.past_warn,
                    if t.past_warn == 1 { "" } else { "s" }
                ),
            ));
            lines.push(kv(
                &format!("past danger (≥{})", humanize_tokens(HARD_DANGER)),
                &format!(
                    "{} session{}",
                    t.past_hard,
                    if t.past_hard == 1 { "" } else { "s" }
                ),
            ));
        }
        lines.push(Line::from(""));

        // ── Pool ─────────────────────────────────────────────────
        lines.push(section_header("pool"));
        let p = &self.stats.pool;
        lines.push(kv(
            "used / idle / max",
            &format!("{} / {} / {}", p.used, p.idle, p.max),
        ));
        if p.max > 0 {
            let pct = (p.used as f64 / p.max as f64) * 100.0;
            lines.push(kv("saturation", &format!("{pct:.0}%")));
        }
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
                .title(" Nerdy · tokens / pool / tables / pgvector / hooks "),
        );
        frame.render_widget(para, area);
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
    if let Ok(agents) = AgentRepo::new(pool).list().await {
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
