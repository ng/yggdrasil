use std::io;
use std::time::Duration;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::prelude::*;
use ratatui::Terminal;
use sqlx::PgPool;

use crate::config::AppConfig;

use super::dashboard::DashboardView;
use super::dag_view::DagView;
use super::eval_view::EvalView;
use super::log_view::LogView;
use super::memgraph_view::MemGraphView;
use super::query_view::QueryView;
use super::tasks_view::TasksView;
use super::trace_view::TraceView;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActiveView {
    Dashboard,
    Dag,
    Tasks,
    Trace,
    Query,
    Logs,
    MemGraph,
    Eval,
}

pub struct App {
    pub active_view: ActiveView,
    pub should_quit: bool,
    pub dashboard: DashboardView,
    pub dag: DagView,
    pub tasks: TasksView,
    pub trace: TraceView,
    pub query: QueryView,
    pub logs: LogView,
    pub memgraph: MemGraphView,
    pub eval: EvalView,
    pub agent_name: String,
    pub query_focus: bool, // true = typing in Query pane; blocks global keys
    /// Recent events shown in the global bottom status bar across all panes.
    pub status_tail: Vec<(String, String, String)>, // (hh:mm:ss, kind, one-line detail)
}

impl App {
    pub fn new(agent_name: String) -> Self {
        Self {
            active_view: ActiveView::Dashboard,
            should_quit: false,
            dashboard: DashboardView::new(),
            dag: DagView::new(),
            tasks: TasksView::new(),
            trace: TraceView::new(),
            query: QueryView::new(),
            logs: LogView::new(),
            memgraph: MemGraphView::new(),
            eval: EvalView::new(),
            agent_name,
            query_focus: false,
            status_tail: Vec::new(),
        }
    }

    /// Pull the 3 most-recent events for the global bottom strip.
    pub async fn refresh_status_tail(&mut self, pool: &PgPool) {
        let rows: Vec<(chrono::DateTime<chrono::Utc>, String, serde_json::Value)> =
            sqlx::query_as(
                "SELECT created_at, event_kind::text, payload
                 FROM events ORDER BY created_at DESC LIMIT 3"
            ).fetch_all(pool).await.unwrap_or_default();
        self.status_tail = rows.into_iter().rev().map(|(t, k, p)| {
            let ts = t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
            (ts, k, short_status_detail(&p))
        }).collect();
    }

    pub async fn handle_key(&mut self, pool: &PgPool, code: KeyCode, modifiers: KeyModifiers) {
        // DAG add-mode ('n' input overlay) captures most keys. Takes
        // precedence over Query focus since they never overlap.
        if self.active_view == ActiveView::Dag && self.dag.add_mode() {
            match code {
                KeyCode::Esc => self.dag.add_cancel(),
                KeyCode::Backspace => self.dag.add_pop(),
                KeyCode::Enter => {
                    if let Some((parent, title)) = self.dag.add_commit() {
                        let agent = std::env::var("YGG_AGENT_NAME").ok()
                            .unwrap_or_else(|| self.agent_name.clone());
                        let result = match parent {
                            Some(p) => crate::cli::plan_cmd::add(
                                pool, &p, &title, None, None, &[], &agent,
                            ).await.map(|_| format!("added under {p}")),
                            None => crate::cli::plan_cmd::create(
                                pool, &title, None, &agent,
                            ).await.map(|t| format!("created epic seq={}", t.seq)),
                        };
                        self.dag.flash = match result {
                            Ok(msg) => msg,
                            Err(e) => format!("add failed: {e}"),
                        };
                        let _ = self.dag.refresh(pool).await;
                    }
                }
                KeyCode::Char(c) => {
                    if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                        self.should_quit = true;
                    } else {
                        self.dag.add_push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        // Query pane is always in input mode while active, so typing
        // characters goes to the input buffer. Esc or Tab/arrows leave;
        // Ctrl-C quits. Enter runs the query.
        if self.query_focus {
            match code {
                KeyCode::Esc => self.set_view(ActiveView::Dashboard),
                KeyCode::Tab | KeyCode::Right => self.cycle_view_forward(),
                KeyCode::BackTab | KeyCode::Left => self.cycle_view_backward(),
                KeyCode::Enter => {
                    let _ = self.query.run_query(pool).await;
                }
                KeyCode::Backspace => self.query.pop_char(),
                KeyCode::Char(c) => {
                    if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                        self.should_quit = true;
                    } else {
                        self.query.push_char(c);
                    }
                }
                KeyCode::Up => self.query.select_prev(),
                KeyCode::Down => self.query.select_next(),
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('1') => self.set_view(ActiveView::Dashboard),
            KeyCode::Char('2') => self.set_view(ActiveView::Dag),
            KeyCode::Char('3') => self.set_view(ActiveView::Tasks),
            KeyCode::Char('4') => self.set_view(ActiveView::Trace),
            KeyCode::Char('5') => self.set_view(ActiveView::Query),
            KeyCode::Char('6') => self.set_view(ActiveView::Logs),
            KeyCode::Char('7') => self.set_view(ActiveView::MemGraph),
            KeyCode::Char('8') => self.set_view(ActiveView::Eval),
            KeyCode::Tab | KeyCode::Right => self.cycle_view_forward(),
            KeyCode::BackTab | KeyCode::Left => self.cycle_view_backward(),
            KeyCode::Char('f') if self.active_view == ActiveView::Logs => {
                self.logs.cycle_filter();
            }
            KeyCode::Char('s') if self.active_view == ActiveView::Dag => {
                self.dag.cycle_sort();
                let _ = self.dag.refresh(pool).await;
            }
            KeyCode::Char('a') if self.active_view == ActiveView::Dag => {
                self.dag.cycle_agent_filter();
                let _ = self.dag.refresh(pool).await;
            }
            KeyCode::Char('f') if self.active_view == ActiveView::Dag => {
                self.dag.toggle_subtree_focus();
                let _ = self.dag.refresh(pool).await;
            }
            KeyCode::Char('c') if self.active_view == ActiveView::Dag => {
                self.dag.clear_filters();
                let _ = self.dag.refresh(pool).await;
            }
            KeyCode::Char('r') if self.active_view == ActiveView::Dag
                && !self.dag.add_mode() =>
            {
                if let Some(task_ref) = self.dag.selected_task_ref() {
                    let agent = std::env::var("YGG_AGENT_NAME")
                        .ok()
                        .unwrap_or_else(|| self.agent_name.clone());
                    let silent = |_: &str| {};
                    match crate::cli::plan_cmd::run_with_reporter(
                        pool, &task_ref, &agent, false, &silent
                    ).await {
                        Ok(headline) => self.dag.flash = headline,
                        Err(e) => self.dag.flash = format!("run failed: {e}"),
                    }
                    let _ = self.dag.refresh(pool).await;
                }
            }
            KeyCode::Char('n') if self.active_view == ActiveView::Dag
                && !self.dag.add_mode() =>
            {
                self.dag.add_begin();
            }
            KeyCode::Char('S') if self.active_view == ActiveView::Dashboard => {
                self.dashboard.toggle_session_scope();
                let _ = self.dashboard.refresh(pool).await;
            }
            KeyCode::Char('w') if self.active_view == ActiveView::Eval => {
                self.eval.cycle_window();
                let _ = self.eval.refresh(pool).await;
            }
            KeyCode::Up => match self.active_view {
                ActiveView::Dag => self.dag.scroll_up(),
                ActiveView::Dashboard => self.dashboard.select_prev(),
                ActiveView::Tasks => self.tasks.select_prev(),
                ActiveView::Trace => self.trace.select_prev(),
                ActiveView::Logs => self.logs.scroll_up(),
                ActiveView::MemGraph => self.memgraph.scroll_up(),
                _ => {}
            },
            KeyCode::Down => match self.active_view {
                ActiveView::Dag => self.dag.scroll_down(),
                ActiveView::Dashboard => self.dashboard.select_next(),
                ActiveView::Tasks => self.tasks.select_next(),
                ActiveView::Trace => self.trace.select_next(),
                ActiveView::Logs => self.logs.scroll_down(),
                ActiveView::MemGraph => self.memgraph.scroll_down(),
                _ => {}
            },
            KeyCode::Enter => match self.active_view {
                ActiveView::Dashboard => {
                    // Jump to DAG with the owner filter pre-set to the
                    // agent whose row was selected. Without this, DAG just
                    // showed the cross-repo view regardless of the click.
                    if let Some(agent) = self.dashboard.selected_agent_full().cloned() {
                        self.dag.agent_filter =
                            super::dag_view::AgentFilter::Specific(agent.agent_id);
                        let _ = self.dag.refresh(pool).await;
                        self.set_view(ActiveView::Dag);
                    }
                }
                ActiveView::Dag => self.dag.toggle_detail(),
                ActiveView::Logs => self.logs.toggle_detail(),
                ActiveView::MemGraph => self.memgraph.toggle_detail(),
                ActiveView::Tasks => {
                    // Enter on the Tasks ready-list executes the selected
                    // task via plan_cmd::run_with_reporter — the reporter
                    // swallows status so the TUI frame isn't corrupted.
                    if let Some(task_ref) = self.tasks.selected_task_ref() {
                        let agent = std::env::var("YGG_AGENT_NAME").ok()
                            .unwrap_or_else(|| self.agent_name.clone());
                        let silent = |_: &str| {};
                        match crate::cli::plan_cmd::run_with_reporter(
                            pool, &task_ref, &agent, false, &silent
                        ).await {
                            Ok(headline) => self.tasks.set_flash(headline),
                            Err(e) => self.tasks.set_flash(format!("run failed: {e}")),
                        }
                        let _ = self.tasks.refresh(pool).await;
                    }
                }
                _ => {}
            },
            KeyCode::Esc => match self.active_view {
                ActiveView::Dag if self.dag.detail_open => self.dag.detail_open = false,
                ActiveView::MemGraph if self.memgraph.detail_open => self.memgraph.detail_open = false,
                _ => {}
            },
            _ => {}
        }
    }

    /// Change the active pane, keeping query_focus in sync. The Query pane
    /// is always in input mode while active — anything else resets focus so
    /// global keybindings work.
    fn set_view(&mut self, v: ActiveView) {
        self.active_view = v;
        self.query_focus = v == ActiveView::Query;
    }
    fn cycle_view_forward(&mut self) {
        let next = match self.active_view {
            ActiveView::Dashboard => ActiveView::Dag,
            ActiveView::Dag => ActiveView::Tasks,
            ActiveView::Tasks => ActiveView::Trace,
            ActiveView::Trace => ActiveView::Query,
            ActiveView::Query => ActiveView::Logs,
            ActiveView::Logs => ActiveView::MemGraph,
            ActiveView::MemGraph => ActiveView::Eval,
            ActiveView::Eval => ActiveView::Dashboard,
        };
        self.set_view(next);
    }
    fn cycle_view_backward(&mut self) {
        let prev = match self.active_view {
            ActiveView::Dashboard => ActiveView::Eval,
            ActiveView::Dag => ActiveView::Dashboard,
            ActiveView::Tasks => ActiveView::Dag,
            ActiveView::Trace => ActiveView::Tasks,
            ActiveView::Query => ActiveView::Trace,
            ActiveView::Logs => ActiveView::Query,
            ActiveView::MemGraph => ActiveView::Logs,
            ActiveView::Eval => ActiveView::MemGraph,
        };
        self.set_view(prev);
    }

    /// Paint the whole TUI into `frame`. Extracted so the run loop can call it
    /// both before and after a refresh — painting before refresh lets each
    /// view's own "loading" state be visible while the DB query blocks.
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),   // tab row
                Constraint::Length(1),   // context-sensitive help row
                Constraint::Min(0),      // active pane
                Constraint::Length(3),   // global status strip (3 recent events)
            ])
            .split(area);

        let tab = |label: &str, active: bool| -> Span<'static> {
            if active {
                Span::styled(format!(" {label} "),
                    Style::default().fg(Color::Black).bg(Color::Cyan))
            } else {
                Span::styled(format!(" {label} "), Style::default().fg(Color::Gray))
            }
        };

        let tabs = vec![
            tab("[1] Dashboard", self.active_view == ActiveView::Dashboard),
            tab("[2] DAG",       self.active_view == ActiveView::Dag),
            tab("[3] Tasks",     self.active_view == ActiveView::Tasks),
            tab("[4] Trace",     self.active_view == ActiveView::Trace),
            tab("[5] Query",     self.active_view == ActiveView::Query),
            tab("[6] Logs",      self.active_view == ActiveView::Logs),
            tab("[7] Memgraph",  self.active_view == ActiveView::MemGraph),
            tab("[8] Eval",      self.active_view == ActiveView::Eval),
        ];
        frame.render_widget(Line::from(tabs), chunks[0]);

        let nav_span = Span::styled(
            "  q=quit  ←→/tab=nav  Enter=detail  ",
            Style::default().fg(Color::DarkGray),
        );
        let pane_hint = match self.active_view {
            ActiveView::Dashboard => "S=session-scope",
            ActiveView::Dag => "s=sort  a=agent  f=focus  c=clear",
            ActiveView::Tasks => "↑↓ select  ·  Enter=run (worktree + CC session)",
            ActiveView::Trace => "↑↓ select",
            ActiveView::Query => "type then Enter  ·  Esc=leave",
            ActiveView::Logs => "f=filter  Enter=detail",
            ActiveView::MemGraph => "↑↓ scroll  Enter=detail  Esc=close",
            ActiveView::Eval => "w=cycle window (1h/6h/24h/7d)",
        };
        let hint_line = Line::from(vec![
            nav_span,
            Span::styled(pane_hint.to_string(),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)),
        ]);
        frame.render_widget(hint_line, chunks[1]);

        match self.active_view {
            ActiveView::Dashboard => self.dashboard.render(frame, chunks[2]),
            ActiveView::Dag       => self.dag.render(frame, chunks[2]),
            ActiveView::Tasks     => self.tasks.render(frame, chunks[2]),
            ActiveView::Trace     => self.trace.render(frame, chunks[2]),
            ActiveView::Query     => self.query.render(frame, chunks[2]),
            ActiveView::Logs      => self.logs.render(frame, chunks[2]),
            ActiveView::MemGraph  => self.memgraph.render(frame, chunks[2]),
            ActiveView::Eval      => self.eval.render(frame, chunks[2]),
        }

        let lines: Vec<Line> = self.status_tail.iter().map(|(ts, kind, detail)| {
            let (glyph, color) = event_glyph(kind);
            Line::from(vec![
                Span::styled(ts.clone(), Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(format!("{kind:<18}"), Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(detail.clone(), Style::default().fg(Color::Gray)),
            ])
        }).collect();
        let status = ratatui::widgets::Paragraph::new(lines)
            .block(ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::TOP)
                .title(" events "));
        frame.render_widget(status, chunks[3]);
    }
}

/// Glyph + color for an event kind — mirrors src/cli/logs_cmd.rs::kind_style.
fn event_glyph(kind: &str) -> (&'static str, Color) {
    match kind {
        "node_written"         => ("●", Color::Green),
        "lock_acquired"        => ("⚿", Color::Yellow),
        "lock_released"        => ("○", Color::DarkGray),
        "digest_written"       => ("◈", Color::Cyan),
        "similarity_hit"       => ("≈", Color::Blue),
        "correction_detected"  => ("✗", Color::Red),
        "hook_fired"           => ("▸", Color::Yellow),
        "embedding_call"       => ("⚡", Color::Cyan),
        "task_created"         => ("✚", Color::Green),
        "task_status_changed"  => ("◆", Color::Yellow),
        "remembered"           => ("♦", Color::Blue),
        "embedding_cache_hit"  => ("⚡", Color::Green),
        "classifier_decision"  => ("⚖", Color::Cyan),
        "scoring_decision"     => ("·", Color::Gray),
        "redaction_applied"    => ("✂", Color::Red),
        "hit_referenced"       => ("✓", Color::Green),
        "agent_state_changed"  => ("↪", Color::Blue),
        _                      => ("·", Color::Gray),
    }
}

fn short_status_detail(p: &serde_json::Value) -> String {
    // One-line detail for the bottom status strip. Best-effort per kind.
    if let Some(score) = p.get("total_score").or_else(|| p.get("similarity"))
        .and_then(|v| v.as_f64())
    {
        let src = p.get("source_agent").and_then(|v| v.as_str()).unwrap_or("?");
        let snip = p.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
        let s = if snip.chars().count() > 40 {
            snip.chars().take(40).collect::<String>() + "…"
        } else { snip.to_string() };
        return format!("score={score:.2} from {src}  {s}");
    }
    if let Some(t) = p.get("turns").and_then(|v| v.as_i64()) {
        return format!("{t} turns");
    }
    if let Some(r) = p.get("ref").and_then(|v| v.as_str()) {
        if let Some(to) = p.get("to").and_then(|v| v.as_str()) {
            return format!("{r} → {to}");
        }
        return r.to_string();
    }
    if let Some(snip) = p.get("snippet").and_then(|v| v.as_str()) {
        let s = if snip.chars().count() > 60 {
            snip.chars().take(60).collect::<String>() + "…"
        } else { snip.to_string() };
        return s;
    }
    String::new()
}

/// Run the TUI event loop.
pub async fn run(pool: &PgPool, _config: &AppConfig) -> Result<(), anyhow::Error> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let agent_name = std::env::var("YGG_AGENT_NAME").unwrap_or_else(|_| {
        std::env::current_dir().ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "ygg".to_string())
    });

    let mut app = App::new(agent_name);

    loop {
        // Draw first so each view can paint its own "loading" state for the
        // duration of the refresh below — otherwise the UI freezes on the
        // previous frame while the DB query blocks. Cost: rendered data is
        // one 500ms tick behind, which matches the refresh cadence anyway.
        terminal.draw(|frame| app.draw(frame))?;

        // Refresh dashboard (cheap) + the global status tail; refresh the active view too.
        app.dashboard.refresh(pool).await?;
        app.refresh_status_tail(pool).await;
        match app.active_view {
            ActiveView::Dag     => { app.dag.refresh(pool).await?; }
            ActiveView::Tasks   => { app.tasks.refresh(pool).await?; }
            ActiveView::Trace   => { app.trace.refresh(pool).await?; }
            ActiveView::Logs    => { app.logs.refresh(pool).await?; }
            ActiveView::MemGraph => { app.memgraph.refresh(pool).await?; }
            ActiveView::Eval    => { app.eval.refresh(pool).await?; }
            _ => {}
        }

        // 500ms poll for key input; refresh loop ticks every 500ms regardless.
        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(pool, key.code, key.modifiers).await;
            }
        }

        if app.should_quit { break; }
    }

    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
