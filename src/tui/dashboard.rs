use chrono::{Duration as CDuration, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use crate::lock::LockManager;
use crate::models::agent::{AgentRepo, AgentWorkflow};

/// Rough token-window estimate from the latest Claude Code transcript for
/// the given agent. Matches the bar/meter logic: find `~/.claude/projects/<slug>/*.jsonl`
/// whose slug ends with the agent name, take the most-recently-modified
/// file's size, divide by 10 bytes-per-token. Returns None if no
/// transcript is found (agent is a DB-only row — e.g. we saw it once but
/// CC hasn't had a session for it).
fn agent_pressure_tokens(agent_name: &str) -> Option<i64> {
    let home = std::env::var("HOME").ok()?;
    let projects = std::path::PathBuf::from(&home).join(".claude/projects");
    if !projects.exists() { return None; }
    let entries = std::fs::read_dir(&projects).ok()?;
    // We look for a directory whose name ends with the agent slug. CC munges
    // absolute paths into `-Users-ng-…-<agent_name>`, so the suffix match
    // is safe.
    let needle = format!("-{agent_name}");
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else { continue };
        if !name.ends_with(&needle) { continue; }
        let Ok(inner) = std::fs::read_dir(entry.path()) else { continue };
        for f in inner.flatten() {
            if f.path().extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            let mt = f.metadata().ok().and_then(|m| m.modified().ok());
            if let Some(t) = mt {
                match &best {
                    None => best = Some((t, f.path())),
                    Some((bt, _)) if t > *bt => best = Some((t, f.path())),
                    _ => {}
                }
            }
        }
    }
    let (_, path) = best?;
    let bytes = std::fs::metadata(&path).ok()?.len() as i64;
    Some(bytes / 10)
}

/// Dashboard view — system pulse at top, agents in the middle,
/// meaningful locks table at bottom.
pub struct DashboardView {
    agents: Vec<AgentWorkflow>,
    locks: Vec<crate::lock::ResourceLock>,
    selected: usize,
    agent_name_by_id: HashMap<Uuid, String>,

    // Pulse numbers
    prompts_1h: i64,
    digests_1h: i64,
    hits_1h: i64,
    cache_hits_24h: i64,
    cache_total_24h: i64,
    redactions_24h: i64,
    prompts_hourly: Vec<u64>, // last 24h, oldest→newest
}

impl DashboardView {
    pub fn new() -> Self {
        Self {
            agents: vec![], locks: vec![], selected: 0,
            agent_name_by_id: HashMap::new(),
            prompts_1h: 0, digests_1h: 0, hits_1h: 0,
            cache_hits_24h: 0, cache_total_24h: 0, redactions_24h: 0,
            prompts_hourly: vec![0; 24],
        }
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let agent_repo = AgentRepo::new(pool);
        self.agents = agent_repo.list().await?;
        self.agent_name_by_id = self.agents.iter()
            .map(|a| (a.agent_id, a.agent_name.clone()))
            .collect();

        let lock_mgr = LockManager::new(pool, 300);
        self.locks = lock_mgr.list_all().await?;

        // Pulse queries — single roundtrip for the small counters.
        let since_1h = Utc::now() - CDuration::hours(1);
        let since_24h = Utc::now() - CDuration::hours(24);
        let (p1h, d1h, h1h): (i64, i64, i64) = sqlx::query_as(
            r#"SELECT
                 COUNT(*) FILTER (WHERE event_kind::text = 'node_written' AND payload->>'kind' = 'user_message'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'digest_written'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'similarity_hit')
               FROM events WHERE created_at >= $1"#
        ).bind(since_1h).fetch_one(pool).await.unwrap_or((0, 0, 0));
        self.prompts_1h = p1h;
        self.digests_1h = d1h;
        self.hits_1h = h1h;

        let (ch24, cc24, r24): (i64, i64, i64) = sqlx::query_as(
            r#"SELECT
                 COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'redaction_applied')
               FROM events WHERE created_at >= $1"#
        ).bind(since_24h).fetch_one(pool).await.unwrap_or((0, 0, 0));
        self.cache_hits_24h = ch24;
        self.cache_total_24h = ch24 + cc24;
        self.redactions_24h = r24;

        // Prompts per hour sparkline, 24h.
        let sparkline: Vec<(i32, i64)> = sqlx::query_as(
            "SELECT (24 - FLOOR(EXTRACT(EPOCH FROM (now() - created_at)) / 3600))::int,
                    COUNT(*)
             FROM events
             WHERE created_at >= now() - interval '24 hours'
               AND event_kind::text = 'node_written'
               AND payload->>'kind' = 'user_message'
             GROUP BY 1 ORDER BY 1"
        ).fetch_all(pool).await.unwrap_or_default();
        let mut series = vec![0u64; 24];
        for (b, n) in sparkline {
            let idx = (b - 1).clamp(0, 23) as usize;
            series[idx] = n as u64;
        }
        self.prompts_hourly = series;

        Ok(())
    }

    pub fn select_next(&mut self) {
        if !self.agents.is_empty() {
            self.selected = (self.selected + 1).min(self.agents.len() - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected_agent(&self) -> Option<String> {
        self.agents.get(self.selected).map(|a| a.agent_name.clone())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Layout: pulse (top) · agents (middle, stretch) · locks (bottom)
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),  // system pulse
                Constraint::Min(8),     // agents
                Constraint::Length(10), // locks
            ])
            .split(area);

        self.render_pulse(frame, chunks[0]);
        self.render_agents_table(frame, chunks[1]);
        self.render_locks_table(frame, chunks[2]);
    }

    fn render_pulse(&self, frame: &mut Frame, area: Rect) {
        // Split: left = prompts sparkline (visual trend first), right = numeric snapshot
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);

        // Sparkline on the left
        let max = *self.prompts_hourly.iter().max().unwrap_or(&1);
        let spark = Sparkline::default()
            .block(Block::default().borders(Borders::ALL)
                .title(format!(" Prompts / hour — 24h  ·  peak {}  ·  ← old · now → ", max)))
            .data(&self.prompts_hourly)
            .max(max.max(1))
            .style(Style::default().fg(Color::Cyan));
        frame.render_widget(spark, cols[0]);

        let cache_rate = if self.cache_total_24h > 0 {
            (self.cache_hits_24h as f64 / self.cache_total_24h as f64 * 100.0) as i64
        } else { 0 };

        let active = self.agents.iter()
            .filter(|a| a.current_state == crate::models::agent::AgentState::Executing)
            .count();

        let lines: Vec<Line> = vec![
            Line::from(vec![
                Span::styled("agents    ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} total · {} active", self.agents.len(), active),
                    Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("last 1h   ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} prompts · {} digests · {} recalls",
                    self.prompts_1h, self.digests_1h, self.hits_1h),
                    Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("last 24h  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("cache {}/{} ({}%) ",
                    self.cache_hits_24h, self.cache_total_24h, cache_rate),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled(format!("· redacted {}", self.redactions_24h),
                    if self.redactions_24h > 0 {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    } else { Style::default().add_modifier(Modifier::BOLD) }),
            ]),
        ];
        let pulse = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" System pulse "));
        frame.render_widget(pulse, cols[1]);
    }

    fn render_agents_table(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("NAME").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Cell::from("STATE"),
            Cell::from("PRESSURE"),
            Cell::from("HEAD"),
            Cell::from("UPDATED"),
        ]).height(1);

        let rows: Vec<Row> = self.agents.iter().enumerate().map(|(i, agent)| {
            let style = if i == self.selected {
                Style::default().bg(Color::DarkGray)
            } else { Style::default() };

            // Humanize: the enum names read like internals ("context_flush")
            // but most of them map to an intuitive one-word label.
            let (base_label, state_color): (&str, Color) = match agent.current_state {
                crate::models::agent::AgentState::Idle           => ("idle",      Color::Gray),
                crate::models::agent::AgentState::Planning       => ("planning",  Color::Cyan),
                crate::models::agent::AgentState::Executing      => ("working",   Color::Green),
                crate::models::agent::AgentState::WaitingTool    => ("tool",      Color::Yellow),
                crate::models::agent::AgentState::ContextFlush   => ("digesting", Color::Magenta),
                crate::models::agent::AgentState::HumanOverride  => ("paused",    Color::Yellow),
                crate::models::agent::AgentState::Mediation      => ("mediating", Color::Cyan),
                crate::models::agent::AgentState::Error          => ("error",     Color::Red),
                crate::models::agent::AgentState::Shutdown       => ("shutdown",  Color::DarkGray),
            };
            // For WaitingTool, append the tool name (e.g. "tool: Bash") from metadata
            let state_label: String = if matches!(agent.current_state, crate::models::agent::AgentState::WaitingTool) {
                match agent.metadata.get("last_tool").and_then(|v| v.as_str()) {
                    Some(t) if !t.is_empty() => format!("{base_label}: {t}"),
                    _ => base_label.to_string(),
                }
            } else {
                base_label.to_string()
            };

            // Pressure comes from the TRANSCRIPT file size, not our
            // agents.context_tokens counter (which only reflects nodes WE
            // write and drifts badly). Falls through to — if no transcript
            // is findable for this agent.
            let limit: i64 = std::env::var("YGG_CONTEXT_LIMIT")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
            let (pressure_bar, pressure_color) = match agent_pressure_tokens(&agent.agent_name) {
                Some(tokens) if limit > 0 => {
                    let pct = ((tokens as f64 / limit as f64) * 100.0).min(999.0) as u32;
                    let blocks = (pct / 10).min(10) as usize;
                    let color = if pct >= 90 { Color::Red }
                                else if pct >= 75 { Color::Yellow }
                                else if pct >= 50 { Color::Cyan }
                                else { Color::Green };
                    (format!("{}{} {}% ({}K)",
                        "█".repeat(blocks), "░".repeat(10 - blocks), pct, tokens / 1000),
                     color)
                }
                _ => ("—".to_string(), Color::DarkGray),
            };

            Row::new(vec![
                Cell::from(agent.agent_name.clone()),
                Cell::from(state_label).style(Style::default().fg(state_color)),
                Cell::from(pressure_bar).style(Style::default().fg(pressure_color)),
                Cell::from(
                    agent.head_node_id.map(|id| id.to_string()[..8].to_string()).unwrap_or_else(|| "—".into()),
                ),
                Cell::from(humanize_since(agent.updated_at)),
            ]).style(style)
        }).collect();

        let table = Table::new(rows, [
            Constraint::Percentage(25),
            Constraint::Percentage(15),
            Constraint::Percentage(25),
            Constraint::Percentage(15),
            Constraint::Percentage(20),
        ])
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Agents "));

        frame.render_widget(table, area);
    }

    fn render_locks_table(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("RESOURCE").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Cell::from("HELD BY"),
            Cell::from("HELD FOR"),
            Cell::from("EXPIRES"),
        ]).height(1);

        let now = Utc::now();
        let rows: Vec<Row> = self.locks.iter().map(|l| {
            let resource = short_resource(&l.resource_key);
            let agent = self.agent_name_by_id.get(&l.agent_id)
                .cloned()
                .unwrap_or_else(|| format!("{}…", &l.agent_id.to_string()[..8]));
            let held = humanize_since(l.acquired_at);
            let ttl_secs = (l.expires_at - now).num_seconds();
            let (ttl_label, ttl_color) = if ttl_secs <= 0 {
                ("expired", Color::Red)
            } else if ttl_secs < 60 {
                ("<1m",    Color::Yellow)
            } else if ttl_secs < 300 {
                ("<5m",    Color::Yellow)
            } else {
                ("ok",     Color::Green)
            };
            // "0s (expired)" reads worse than just "expired".
            let ttl_str = if ttl_secs <= 0 {
                "expired".to_string()
            } else {
                format!("{}  ({})", humanize_duration(ttl_secs), ttl_label)
            };
            let ttl = (ttl_label, ttl_color);

            Row::new(vec![
                Cell::from(resource),
                Cell::from(agent).style(Style::default().fg(Color::Cyan)),
                Cell::from(held).style(Style::default().fg(Color::DarkGray)),
                Cell::from(ttl_str).style(Style::default().fg(ttl.1)),
            ])
        }).collect();

        let title = format!(" Locks — {} held (advisory) ", self.locks.len());
        let table = Table::new(rows, [
            Constraint::Percentage(45),
            Constraint::Percentage(20),
            Constraint::Percentage(15),
            Constraint::Percentage(20),
        ])
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));

        frame.render_widget(table, area);
    }
}

/// Trim a resource key (usually an abs path) to the last two path
/// components plus an ellipsis prefix. Keeps the informative tail visible.
fn short_resource(s: &str) -> String {
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 { return s.to_string(); }
    let tail = parts[parts.len() - 2..].join("/");
    format!("…/{tail}")
}

fn humanize_since(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0);
    humanize_duration(secs) + " ago"
}

fn humanize_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else if secs < 86400 { format!("{}h", secs / 3600) }
    else { format!("{}d", secs / 86400) }
}
