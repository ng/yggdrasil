use chrono::{Duration as CDuration, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table};
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::lock::LockManager;
use crate::models::agent::{AgentRepo, AgentWorkflow};

const PRESSURE_TTL: Duration = Duration::from_secs(5);

/// Actual context-window tokens for the given agent, extracted from the
/// latest `usage` block in its Claude Code transcript. This is the same
/// number CC's own status line shows: cache_read + cache_creation + output.
/// Falls back to a file-size estimate only if no usage block is found.
fn agent_pressure_tokens(agent_name: &str) -> Option<i64> {
    let home = std::env::var("HOME").ok()?;
    let projects = std::path::PathBuf::from(&home).join(".claude/projects");
    if !projects.exists() { return None; }
    let entries = std::fs::read_dir(&projects).ok()?;
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

    if let Some(tokens) = parse_last_usage_tokens(&path) {
        return Some(tokens);
    }
    // Only fall back if we couldn't find any usage entry — 30 bytes/token
    // is closer to reality for JSONL than the old 10 (those were 5-6x high).
    let bytes = std::fs::metadata(&path).ok()?.len() as i64;
    Some(bytes / 30)
}

/// Walk the JSONL from end to start looking for the last `usage` object
/// and sum the fields that count against the context window.
fn parse_last_usage_tokens(path: &std::path::Path) -> Option<i64> {
    // Read the last 200KB — usage blocks are always near the tail, and we
    // avoid reading multi-MB transcripts in full on every refresh.
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let tail_start = len.saturating_sub(200_000);
    file.seek(SeekFrom::Start(tail_start)).ok()?;
    let mut buf = String::new();
    file.take(200_000).read_to_string(&mut buf).ok()?;

    // Scan lines in reverse order for the first JSON with a usable usage block.
    for line in buf.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        // Usage can appear nested under message.usage (assistant entries) or
        // at the top level, depending on the record shape.
        let usage = v.pointer("/message/usage")
            .or_else(|| v.pointer("/usage"));
        let Some(u) = usage else { continue };
        let cr = u.get("cache_read_input_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let cc = u.get("cache_creation_input_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let inp = u.get("input_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let out = u.get("output_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let total = cr + cc + inp + out;
        if total > 0 { return Some(total); }
    }
    None
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
    prompts_hourly: Vec<u64>,                 // global, last 24h, oldest→newest
    per_agent_hourly: HashMap<Uuid, [u64; 24]>, // per agent, last 24h
    recent_transitions: Vec<(chrono::DateTime<chrono::Utc>, String, String, String, Option<String>)>,
    /// Live workers. Separate agent / persona / state so the panel can
    /// render them as proper columns instead of one squashed label.
    /// (task_ref, agent, persona, state_glyph, state_color, title, started_at, tmux_window)
    workers: Vec<WorkerRow>,
    /// Cursor position in the Workers panel — used by the Enter-to-attach
    /// keybind. Only moves when Workers has focus (see DashboardFocus).
    pub worker_sel: usize,
    pub focus: DashboardFocus,

    /// Transcript-file-size pressure cache. Reading ~/.claude/projects on
    /// every 500ms render is expensive; cache per-agent for PRESSURE_TTL.
    pressure_cache: HashMap<String, (Instant, Option<i64>)>,

    /// When on, pulse queries filter to the most-recent cc_session_id.
    /// Toggled with `S` on the dashboard pane.
    pub session_scoped: bool,
    /// The session id the pulse is currently scoped to, populated on refresh.
    pub current_session_id: Option<String>,
    /// Live session count per agent, refreshed with the rest of the state.
    pub live_sessions_by_agent: HashMap<Uuid, i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DashboardFocus {
    Agents,
    Workers,
}

#[derive(Debug, Clone)]
pub struct WorkerRow {
    pub task_ref: String,
    pub agent: String,
    pub persona: Option<String>,
    pub state: String,
    pub title: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub tmux_session: String,
    pub tmux_window: String,
}

impl DashboardView {
    pub fn new() -> Self {
        Self {
            agents: vec![], locks: vec![], selected: 0,
            agent_name_by_id: HashMap::new(),
            prompts_1h: 0, digests_1h: 0, hits_1h: 0,
            cache_hits_24h: 0, cache_total_24h: 0, redactions_24h: 0,
            prompts_hourly: vec![0; 24],
            per_agent_hourly: HashMap::new(),
            recent_transitions: Vec::new(),
            workers: Vec::new(),
            worker_sel: 0,
            focus: DashboardFocus::Agents,
            pressure_cache: HashMap::new(),
            session_scoped: false,
            current_session_id: None,
            live_sessions_by_agent: HashMap::new(),
        }
    }

    pub fn toggle_session_scope(&mut self) { self.session_scoped = !self.session_scoped; }

    fn cached_pressure(&mut self, agent_name: &str) -> Option<i64> {
        let now = Instant::now();
        if let Some((t, v)) = self.pressure_cache.get(agent_name) {
            if now.duration_since(*t) < PRESSURE_TTL { return *v; }
        }
        let v = agent_pressure_tokens(agent_name);
        self.pressure_cache.insert(agent_name.to_string(), (now, v));
        v
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let agent_repo = AgentRepo::new(pool);
        self.agents = agent_repo.list().await?;
        self.agent_name_by_id = self.agents.iter()
            .map(|a| (a.agent_id, a.agent_name.clone()))
            .collect();

        let lock_mgr = LockManager::new(pool, 300);
        self.locks = lock_mgr.list_all().await?;

        // Live session counts per agent — surfaces when one identity has
        // multiple concurrent CC sessions racing on the same state.
        self.live_sessions_by_agent = crate::models::session::SessionRepo::new(pool)
            .live_counts().await.unwrap_or_default()
            .into_iter().collect();

        // When session-scoped, lock queries to the most-recent CC session
        // that's emitted events in the last 6h. Avoids showing a freshly-
        // closed session forever; stays sticky while one is active.
        self.current_session_id = if self.session_scoped {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT cc_session_id FROM events
                 WHERE cc_session_id IS NOT NULL
                   AND created_at > now() - interval '6 hours'
                 ORDER BY created_at DESC LIMIT 1"
            ).fetch_optional(pool).await.ok().flatten().flatten()
        } else { None };

        // Pulse queries — single roundtrip for the small counters. When a
        // session is in scope we replace the time window with a session
        // match, so the numbers reflect "this session" instead of "last hour
        // globally".
        let since_1h = Utc::now() - CDuration::hours(1);
        let since_24h = Utc::now() - CDuration::hours(24);
        let sid = self.current_session_id.clone();

        let (p1h, d1h, h1h): (i64, i64, i64) = if let Some(sid) = sid.as_deref() {
            sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind::text = 'node_written' AND payload->>'kind' = 'user_message'),
                     COUNT(*) FILTER (WHERE event_kind::text = 'digest_written'),
                     COUNT(*) FILTER (WHERE event_kind::text = 'similarity_hit')
                   FROM events WHERE cc_session_id = $1"#
            ).bind(sid).fetch_one(pool).await.unwrap_or((0, 0, 0))
        } else {
            sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind::text = 'node_written' AND payload->>'kind' = 'user_message'),
                     COUNT(*) FILTER (WHERE event_kind::text = 'digest_written'),
                     COUNT(*) FILTER (WHERE event_kind::text = 'similarity_hit')
                   FROM events WHERE created_at >= $1"#
            ).bind(since_1h).fetch_one(pool).await.unwrap_or((0, 0, 0))
        };
        self.prompts_1h = p1h;
        self.digests_1h = d1h;
        self.hits_1h = h1h;

        let (ch24, cc24, r24): (i64, i64, i64) = if let Some(sid) = sid.as_deref() {
            sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit'),
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'),
                     COUNT(*) FILTER (WHERE event_kind::text = 'redaction_applied')
                   FROM events WHERE cc_session_id = $1"#
            ).bind(sid).fetch_one(pool).await.unwrap_or((0, 0, 0))
        } else {
            sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit'),
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'),
                     COUNT(*) FILTER (WHERE event_kind::text = 'redaction_applied')
                   FROM events WHERE created_at >= $1"#
            ).bind(since_24h).fetch_one(pool).await.unwrap_or((0, 0, 0))
        };
        self.cache_hits_24h = ch24;
        self.cache_total_24h = ch24 + cc24;
        self.redactions_24h = r24;

        // Prompts per hour sparkline, 24h — global across agents.
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

        // Per-agent prompts-per-hour sparkline, 24h — one row per (agent, bucket).
        let per_agent: Vec<(Uuid, i32, i64)> = sqlx::query_as(
            "SELECT agent_id,
                    (24 - FLOOR(EXTRACT(EPOCH FROM (now() - created_at)) / 3600))::int,
                    COUNT(*)
             FROM events
             WHERE created_at >= now() - interval '24 hours'
               AND event_kind::text = 'node_written'
               AND payload->>'kind' = 'user_message'
               AND agent_id IS NOT NULL
             GROUP BY 1, 2"
        ).fetch_all(pool).await.unwrap_or_default();
        self.per_agent_hourly.clear();
        for (aid, b, n) in per_agent {
            let idx = (b - 1).clamp(0, 23) as usize;
            let entry = self.per_agent_hourly.entry(aid).or_insert([0u64; 24]);
            entry[idx] = n as u64;
        }

        // Last 5 agent state transitions for the timeline widget.
        let trans: Vec<(chrono::DateTime<chrono::Utc>, String, serde_json::Value)> =
            sqlx::query_as(
                "SELECT created_at, agent_name, payload FROM events
                 WHERE event_kind = 'agent_state_changed'
                 ORDER BY created_at DESC LIMIT 5"
            ).fetch_all(pool).await.unwrap_or_default();
        self.recent_transitions = trans.into_iter().map(|(ts, name, p)| {
            let from = p.get("from").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let to   = p.get("to").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let tool = p.get("tool").and_then(|v| v.as_str()).map(String::from);
            (ts, name, from, to, tool)
        }).collect();

        // Workers panel reads from the workers table. Observer
        // (yggdrasil-51) maintains state; we just show it.
        let worker_rows: Vec<(String, i32, String, Option<String>, Option<String>, String, chrono::DateTime<chrono::Utc>, String, String)> =
            sqlx::query_as(
                r#"SELECT r.task_prefix, t.seq, t.title,
                          a.agent_name, a.persona,
                          w.state::text, w.started_at,
                          w.tmux_session, w.tmux_window
                     FROM workers w
                     JOIN tasks t ON t.task_id = w.task_id
                     JOIN repos r ON r.repo_id = t.repo_id
                     LEFT JOIN agents a ON a.agent_id = t.assignee
                    WHERE w.ended_at IS NULL
                    ORDER BY w.started_at DESC
                    LIMIT 10"#,
            ).fetch_all(pool).await.unwrap_or_default();
        self.workers = worker_rows.into_iter().map(|(prefix, seq, title, agent, persona, state, ts, ts_sess, ts_win)| {
            WorkerRow {
                task_ref: format!("{prefix}-{seq}"),
                agent: agent.unwrap_or_else(|| "unassigned".into()),
                persona,
                state,
                title,
                started_at: ts,
                tmux_session: ts_sess,
                tmux_window: ts_win,
            }
        }).collect();
        if self.worker_sel >= self.workers.len() {
            self.worker_sel = self.workers.len().saturating_sub(1);
        }

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

    /// Full selected-agent record (needed to set DAG filter by agent_id).
    pub fn selected_agent_full(&self) -> Option<&AgentWorkflow> {
        self.agents.get(self.selected)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Layout: alerts · pulse · agents (stretch) · transitions · locks
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),  // alerts
                Constraint::Length(6),  // system pulse
                Constraint::Min(8),     // agents
                Constraint::Length(6),  // workers (click-to-do spawns)
                Constraint::Length(7),  // state transitions
                Constraint::Length(9),  // locks (trimmed 1 row for workers)
            ])
            .split(area);

        self.render_alerts(frame, chunks[0]);
        self.render_pulse(frame, chunks[1]);
        self.render_agents_table(frame, chunks[2]);
        self.render_workers(frame, chunks[3]);
        self.render_transitions(frame, chunks[4]);
        self.render_locks_table(frame, chunks[5]);
    }

    fn render_workers(&self, frame: &mut Frame, area: Rect) {
        if self.workers.is_empty() {
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  · no workers running — spawn one with `r` on DAG or Enter on Tasks",
                    Style::default().fg(Color::DarkGray),
                )),
            ]).block(Block::default().borders(Borders::ALL).title(" Workers "));
            frame.render_widget(para, area);
            return;
        }
        let now = Utc::now();
        let focused = self.focus == DashboardFocus::Workers;
        let lines: Vec<Line> = self.workers.iter().enumerate().map(|(i, w)| {
            let age = humanize_duration((now - w.started_at).num_seconds().max(0));
            let (g, c) = worker_state_style(&w.state);
            let is_cursor = focused && i == self.worker_sel;
            let cursor = if is_cursor { "▸ " } else { "  " };
            // needs_attention rows highlight with a yellow background so
            // they catch the eye even at the edge of your vision.
            let needs_attn = w.state == "needs_attention";
            let title_style = if needs_attn {
                Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else if is_cursor {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(cursor, Style::default().fg(Color::Cyan)),
                Span::styled(format!("{g} "), Style::default().fg(c).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{:<16}", w.task_ref),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{:<16}", short_cell(&w.agent, 16)),
                    Style::default().fg(Color::White)),
                Span::styled(format!("{:<12}",
                    w.persona.as_deref().map(|p| short_cell(p, 12)).unwrap_or_else(|| "—".into())),
                    Style::default().fg(Color::Magenta)),
                Span::styled(format!("{:<16}", w.state),
                    Style::default().fg(c)),
                Span::styled(format!("{age:<6}"), Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(short_title(&w.title), title_style),
            ])
        }).collect();
        let title = format!(" Workers — {} running  ·  w=focus  ↑↓ scroll  Enter=attach ",
            self.workers.len());
        let block = Block::default()
            .borders(Borders::ALL).title(title)
            .border_style(if focused {
                Style::default().fg(Color::Cyan)
            } else { Style::default() });
        let para = Paragraph::new(lines).block(block);
        frame.render_widget(para, area);
    }

    pub fn workers_focus(&mut self) {
        self.focus = DashboardFocus::Workers;
        if self.worker_sel >= self.workers.len() {
            self.worker_sel = 0;
        }
    }
    pub fn agents_focus(&mut self) { self.focus = DashboardFocus::Agents; }

    pub fn worker_up(&mut self) {
        if self.workers.is_empty() { return; }
        self.worker_sel = if self.worker_sel == 0 {
            self.workers.len() - 1
        } else { self.worker_sel - 1 };
    }
    pub fn worker_down(&mut self) {
        if self.workers.is_empty() { return; }
        self.worker_sel = (self.worker_sel + 1) % self.workers.len();
    }
    pub fn selected_worker(&self) -> Option<&WorkerRow> {
        self.workers.get(self.worker_sel)
    }

    fn render_transitions(&self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line> = if self.recent_transitions.is_empty() {
            vec![Line::from(Span::styled(
                "  · no agent state changes yet — transitions appear here when hooks fire",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            self.recent_transitions.iter().map(|(ts, name, from, to, tool)| {
                let t = ts.with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
                let to_color = state_color(to);
                let mut spans = vec![
                    Span::styled(t, Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::styled(name.clone(), Style::default().fg(Color::Cyan)),
                    Span::raw("  "),
                    Span::styled(from.clone(), Style::default().fg(Color::DarkGray)),
                    Span::raw(" → "),
                    Span::styled(to.clone(), Style::default().fg(to_color).add_modifier(Modifier::BOLD)),
                ];
                if let Some(t) = tool {
                    spans.push(Span::styled(format!(" ({t})"),
                        Style::default().fg(Color::Yellow)));
                }
                Line::from(spans)
            }).collect()
        };
        let para = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Recent state transitions "));
        frame.render_widget(para, area);
    }

    fn render_alerts(&mut self, frame: &mut Frame, area: Rect) {
        let mut alerts: Vec<Span> = Vec::new();
        let limit: i64 = std::env::var("YGG_CONTEXT_LIMIT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);

        // High pressure agents (>=90%)
        let agent_names: Vec<String> = self.agents.iter().map(|a| a.agent_name.clone()).collect();
        for name in &agent_names {
            if let Some(tokens) = self.cached_pressure(name) {
                if limit > 0 {
                    let pct = ((tokens as f64 / limit as f64) * 100.0) as u32;
                    if pct >= 90 {
                        alerts.push(Span::styled(
                            format!(" ⛔ {name} {pct}% "),
                            Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD),
                        ));
                        alerts.push(Span::raw("  "));
                    } else if pct >= 75 {
                        alerts.push(Span::styled(
                            format!(" ⚠ {name} {pct}% "),
                            Style::default().fg(Color::Black).bg(Color::Yellow),
                        ));
                        alerts.push(Span::raw("  "));
                    }
                }
            }
        }

        // Locks held "too long" — 30m+ is our threshold for "someone's probably
        // stuck". Expired locks are just stale state and not interesting.
        let now = Utc::now();
        let long_held = self.locks.iter()
            .filter(|l| (l.expires_at - now).num_seconds() > 0)  // still live
            .filter(|l| (now - l.acquired_at).num_minutes() >= 30)
            .count();
        if long_held > 0 {
            alerts.push(Span::styled(
                format!(" ⏳ {long_held} lock{} held 30m+ ", if long_held == 1 { "" } else { "s" }),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));
            alerts.push(Span::raw("  "));
        }

        // Error-state agents
        let errored = self.agents.iter()
            .filter(|a| a.current_state == crate::models::agent::AgentState::Error).count();
        if errored > 0 {
            alerts.push(Span::styled(
                format!(" ✗ {errored} agent(s) in error "),
                Style::default().fg(Color::White).bg(Color::Red),
            ));
            alerts.push(Span::raw("  "));
        }

        // Redactions in last 24h — informational, not a blocker.
        if self.redactions_24h > 0 {
            alerts.push(Span::styled(
                format!(" 🔒 {} redaction(s) 24h ", self.redactions_24h),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));
        }

        let line = if alerts.is_empty() {
            Line::from(vec![Span::styled(
                "  ✓ all clear — no pressure alerts, no expired locks, no errored agents",
                Style::default().fg(Color::Green),
            )])
        } else {
            Line::from(alerts)
        };

        let para = Paragraph::new(vec![line])
            .block(Block::default().borders(Borders::ALL).title(" Alerts "));
        frame.render_widget(para, area);
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

        // Session-scope indicator lives in the labels — we re-use the same
        // counter fields but relabel "last 1h / last 24h" to just "session"
        // when scoped, so the numbers aren't misleading.
        let counter_label = if self.current_session_id.is_some() { "session   " } else { "last 1h   " };
        let totals_label  = if self.current_session_id.is_some() { "session   " } else { "last 24h  " };

        let lines: Vec<Line> = vec![
            Line::from(vec![
                Span::styled("agents    ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} total · {} active", self.agents.len(), active),
                    Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled(counter_label, Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} prompts · {} digests · {} recalls",
                    self.prompts_1h, self.digests_1h, self.hits_1h),
                    Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled(totals_label, Style::default().fg(Color::DarkGray)),
                Span::styled(format!("cache {}/{} ({}%) ",
                    self.cache_hits_24h, self.cache_total_24h, cache_rate),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled(format!("· redacted {}", self.redactions_24h),
                    if self.redactions_24h > 0 {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    } else { Style::default().add_modifier(Modifier::BOLD) }),
            ]),
        ];
        let title = match &self.current_session_id {
            Some(sid) => {
                let head: String = sid.chars().take(8).collect();
                format!(" System pulse — session {head}…  (S=global) ")
            }
            None => {
                let hint = if self.session_scoped {
                    " — no recent session  (S=global) "
                } else { "  (S=session) " };
                format!(" System pulse{} ", hint)
            }
        };
        let pulse = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(pulse, cols[1]);
    }

    fn render_agents_table(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("NAME").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Cell::from("STATE"),
            Cell::from("PRESSURE"),
            Cell::from("24H"),
            Cell::from("UPDATED"),
        ]).height(1);

        // Shared y-axis for comparable sparklines across agents.
        let global_max = self.per_agent_hourly.values()
            .flat_map(|s| s.iter().copied())
            .max().unwrap_or(1)
            .max(1);

        // Pre-compute cached pressure per agent — we can't call &mut self
        // inside the iter closure below.
        let mut pressure_by_name: HashMap<String, Option<i64>> = HashMap::new();
        let agent_names: Vec<String> = self.agents.iter().map(|a| a.agent_name.clone()).collect();
        for n in &agent_names {
            pressure_by_name.insert(n.clone(), self.cached_pressure(n));
        }

        let selected = self.selected;
        let per_agent = &self.per_agent_hourly;
        let rows: Vec<Row> = self.agents.iter().enumerate().map(|(i, agent)| {
            let style = if i == selected {
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
            let mut state_label: String = if matches!(agent.current_state, crate::models::agent::AgentState::WaitingTool) {
                match agent.metadata.get("last_tool").and_then(|v| v.as_str()) {
                    Some(t) if !t.is_empty() => format!("{base_label}: {t}"),
                    _ => base_label.to_string(),
                }
            } else {
                base_label.to_string()
            };
            // Staleness: if the row says we're active but hasn't been
            // updated in 10+ minutes, the CC session is probably dead.
            // Visual-only — no mutation — dim color + "(stale)" suffix
            // so a dead "tool: Bash" doesn't read identical to a live one.
            let is_active_state = matches!(
                agent.current_state,
                crate::models::agent::AgentState::Executing
                  | crate::models::agent::AgentState::WaitingTool
                  | crate::models::agent::AgentState::Planning
                  | crate::models::agent::AgentState::ContextFlush
            );
            let idle_mins = (Utc::now() - agent.updated_at).num_minutes();
            let state_color = if is_active_state && idle_mins >= 10 {
                state_label.push_str(" (stale)");
                Color::DarkGray
            } else {
                state_color
            };

            // Pressure comes from the TRANSCRIPT file size, not our
            // agents.context_tokens counter (which only reflects nodes WE
            // write and drifts badly). Falls through to — if no transcript
            // is findable for this agent.
            let limit: i64 = std::env::var("YGG_CONTEXT_LIMIT")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
            let (pressure_bar, pressure_color) = match pressure_by_name.get(&agent.agent_name).copied().flatten() {
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

            let sparkline = per_agent.get(&agent.agent_id)
                .map(|s| text_sparkline(s, global_max))
                .unwrap_or_else(|| "        ".to_string());

            // Attach a "×N" badge when an agent has >1 live CC session —
            // means multiple windows are racing on the same identity.
            // Persona, when set, appears as " :role" so you can tell two
            // personas of the same repo apart at a glance.
            let live = self.live_sessions_by_agent.get(&agent.agent_id).copied().unwrap_or(0);
            let base = match &agent.persona {
                Some(p) if !p.is_empty() => format!("{} :{p}", agent.agent_name),
                _ => agent.agent_name.clone(),
            };
            let name_cell = if live > 1 {
                format!("{base}  ×{live}")
            } else { base };

            Row::new(vec![
                Cell::from(name_cell),
                Cell::from(state_label).style(Style::default().fg(state_color)),
                Cell::from(pressure_bar).style(Style::default().fg(pressure_color)),
                Cell::from(sparkline).style(Style::default().fg(Color::Cyan)),
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
            Cell::from("TTL"),
        ]).height(1);

        let now = Utc::now();
        // Drop expired — that's stale state, not a signal to act on. Sort the
        // rest by held-for DESC so the "who's stuck?" entries rise to the top.
        let mut live: Vec<&crate::lock::ResourceLock> = self.locks.iter()
            .filter(|l| (l.expires_at - now).num_seconds() > 0)
            .collect();
        live.sort_by_key(|l| std::cmp::Reverse((now - l.acquired_at).num_seconds()));

        let rows: Vec<Row> = live.iter().map(|l| {
            let resource = short_resource(&l.resource_key);
            let agent = self.agent_name_by_id.get(&l.agent_id)
                .cloned()
                .unwrap_or_else(|| format!("{}…", &l.agent_id.to_string()[..8]));
            let held_secs = (now - l.acquired_at).num_seconds().max(0);
            let held = humanize_duration(held_secs);
            // Color-code held-for: >30m = yellow (possibly stuck), >2h = red
            let held_color = if held_secs > 7200 { Color::Red }
                             else if held_secs > 1800 { Color::Yellow }
                             else { Color::DarkGray };

            let ttl_secs = (l.expires_at - now).num_seconds();
            let ttl_color = if ttl_secs < 60 { Color::Yellow }
                            else if ttl_secs < 300 { Color::Yellow }
                            else { Color::Green };

            Row::new(vec![
                Cell::from(resource),
                Cell::from(agent).style(Style::default().fg(Color::Cyan)),
                Cell::from(held).style(Style::default().fg(held_color)),
                Cell::from(humanize_duration(ttl_secs)).style(Style::default().fg(ttl_color)),
            ])
        }).collect();

        let title = format!(" Locks — {} live (sorted by longest held) ", live.len());
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

fn state_color(s: &str) -> Color {
    match s {
        "idle" => Color::Gray,
        "executing" | "working" => Color::Green,
        "waiting_tool" | "tool" => Color::Yellow,
        "context_flush" | "digesting" => Color::Magenta,
        "error" => Color::Red,
        "human_override" | "paused" => Color::Yellow,
        "planning" | "mediation" => Color::Cyan,
        _ => Color::Gray,
    }
}

/// Render a 24-hour series as block-char sparkline. `max` is the shared y-axis
/// so values across rows are visually comparable.
fn text_sparkline(series: &[u64], max: u64) -> String {
    // 8-step bar chars, empty cell for zero so sparse agents look sparse.
    const BARS: [char; 8] = ['▁','▂','▃','▄','▅','▆','▇','█'];
    let max = max.max(1);
    series.iter().map(|&v| {
        if v == 0 { ' ' }
        else {
            let step = ((v * 7 + max - 1) / max).min(7) as usize;
            BARS[step]
        }
    }).collect()
}

fn short_title(s: &str) -> String {
    if s.chars().count() <= 60 { s.to_string() }
    else { s.chars().take(60).collect::<String>() + "…" }
}

fn short_cell(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() }
    else { s.chars().take(max.saturating_sub(1)).collect::<String>() + "…" }
}

fn worker_state_style(state: &str) -> (&'static str, Color) {
    match state {
        "spawned"         => ("◌", Color::DarkGray),
        "running"         => ("▶", Color::Green),
        "idle"            => ("•", Color::Gray),
        "needs_attention" => ("⚠", Color::Yellow),
        "completed"       => ("✓", Color::DarkGray),
        "failed"          => ("✗", Color::Red),
        "abandoned"       => ("⊘", Color::DarkGray),
        _                 => ("?", Color::Gray),
    }
}

fn humanize_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else if secs < 86400 { format!("{}h", secs / 3600) }
    else { format!("{}d", secs / 86400) }
}
