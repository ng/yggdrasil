use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::prelude::*;
use sqlx::PgPool;
use std::io;
use std::time::Duration;

use crate::config::AppConfig;

use super::dag_view::DagView;
use super::dashboard::DashboardView;
use super::eval_view::EvalView;
use super::locks_view::LocksView;
use super::log_view::LogView;
use super::memgraph_view::MemGraphView;
use super::prompt_view::PromptView;
use super::query_view::QueryView;
use super::runs_view::RunsView;
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
    Prompt,
    Locks,
    Runs,
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
    pub prompt: PromptView,
    pub locks: LocksView,
    pub runs: RunsView,
    pub agent_name: String,
    pub query_focus: bool, // true = typing in Query pane; blocks global keys
    /// Recent events shown in the global bottom status bar across all panes.
    pub status_tail: Vec<(String, String, String)>, // (hh:mm:ss, kind, one-line detail)
    /// Right-hand-side orchestration stats (filled each refresh tick).
    pub ops_stats: OpsStats,
    /// When Enter on the Workers panel fires, we defer the actual tmux
    /// attach until after ratatui's alternate-screen teardown to avoid
    /// a half-mode-switched terminal. Set here, read in `run` post-loop.
    pub attach_pending: Option<(String, String)>, // (session, window)
    /// Rolling history of `agents_alive` over the last
    /// `SPARK_HISTORY_LEN` refreshes (yggdrasil-150). Rendered as a
    /// Unicode block sparkline in the orchestration panel so liveness
    /// trend is visible at a glance.
    pub alive_history: SparkBuffer,
}

/// How many recent samples to keep for in-panel sparklines. 30 samples
/// at the existing 500ms refresh tick = 15 s of history, the sweet spot
/// between "useful trend" and "fits on a single 10-glyph cell".
pub const SPARK_HISTORY_LEN: usize = 30;

/// yggdrasil-150: bounded ring buffer feeding a Unicode block sparkline.
/// Pushes drop the oldest sample when the cap is reached. `glyphs(width)`
/// renders the last `width` samples normalized against the largest value
/// in the buffer; an empty or all-zero buffer renders to spaces.
#[derive(Debug, Clone)]
pub struct SparkBuffer {
    pub samples: std::collections::VecDeque<u64>,
    pub cap: usize,
}

impl SparkBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, value: u64) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    /// Render the most recent `width` samples as Unicode block glyphs.
    /// Empty buffer returns `width` spaces so the sparkline never shifts
    /// the surrounding layout.
    pub fn glyphs(&self, width: usize) -> String {
        const LEVELS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        if width == 0 {
            return String::new();
        }
        if self.samples.is_empty() {
            return " ".repeat(width);
        }
        let take_n = width.min(self.samples.len());
        let pad_n = width - take_n;
        let max = *self.samples.iter().max().unwrap_or(&0).max(&1);
        let start = self.samples.len() - take_n;
        let mut out = " ".repeat(pad_n);
        for &v in self.samples.iter().skip(start) {
            // Map [0..=max] → [0..=8] glyph index. saturating_mul keeps
            // pathological max=u64::MAX cases from panicking.
            let idx = if max == 0 {
                0
            } else {
                ((v.saturating_mul(8)) / max).min(8) as usize
            };
            out.push(LEVELS[idx]);
        }
        out
    }
}

/// Lightweight orchestration snapshot rendered on the right side of the
/// global status strip. Cheap queries — every 500ms tick is fine.
#[derive(Default, Clone)]
pub struct OpsStats {
    pub agents_alive: i64,  // != idle and updated in last 10m
    pub agents_stuck: i64,  // active-state but updated > 10m ago
    pub tasks_running: i64, // tasks.status = in_progress
    pub live_sessions: i64, // sessions.ended_at IS NULL
    pub ollama_ok: bool,
    pub db_ms: u64, // round-trip ping
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
            prompt: PromptView::new(),
            locks: LocksView::new(),
            runs: RunsView::new(),
            agent_name,
            query_focus: false,
            status_tail: Vec::new(),
            ops_stats: OpsStats::default(),
            attach_pending: None,
            alive_history: SparkBuffer::new(SPARK_HISTORY_LEN),
        }
    }

    /// Pull the 3 most-recent events for the global bottom strip.
    pub async fn refresh_status_tail(&mut self, pool: &PgPool) {
        let rows: Vec<(chrono::DateTime<chrono::Utc>, String, serde_json::Value)> = sqlx::query_as(
            "SELECT created_at, event_kind::text, payload
                 FROM events ORDER BY created_at DESC LIMIT 3",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        self.status_tail = rows
            .into_iter()
            .rev()
            .map(|(t, k, p)| {
                let ts = t
                    .with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string();
                (ts, k, short_status_detail(&p))
            })
            .collect();

        // Orchestration snapshot. One roundtrip: agents live/stuck, tasks
        // running, sessions live. Ollama health is a bounded HTTP ping so
        // it can't block the tick.
        let db_start = std::time::Instant::now();
        let (alive, stuck, running, sessions): (i64, i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT
              (SELECT COUNT(*) FROM agents
                WHERE archived_at IS NULL
                  AND current_state <> 'idle'
                  AND updated_at >= now() - interval '10 minutes'),
              (SELECT COUNT(*) FROM agents
                WHERE archived_at IS NULL
                  AND current_state IN ('executing','waiting_tool','planning','context_flush')
                  AND updated_at <  now() - interval '10 minutes'),
              (SELECT COUNT(*) FROM tasks WHERE status = 'in_progress'),
              (SELECT COUNT(*) FROM sessions WHERE ended_at IS NULL)
            "#,
        )
        .fetch_one(pool)
        .await
        .unwrap_or((0, 0, 0, 0));
        self.ops_stats.agents_alive = alive;
        self.ops_stats.agents_stuck = stuck;
        self.ops_stats.tasks_running = running;
        self.ops_stats.live_sessions = sessions;
        self.ops_stats.db_ms = db_start.elapsed().as_millis() as u64;
        // yggdrasil-150: feed the in-panel sparkline.
        self.alive_history.push(alive.max(0) as u64);

        // Tight 150ms timeout — local Ollama answers in <20ms when
        // running; 150ms is plenty to catch "it's alive" without
        // stalling the refresh if it's down.
        self.ops_stats.ollama_ok =
            tokio::time::timeout(std::time::Duration::from_millis(150), reqwest_ping())
                .await
                .unwrap_or(false);
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
                        let agent = std::env::var("YGG_AGENT_NAME")
                            .ok()
                            .unwrap_or_else(|| self.agent_name.clone());
                        let result = match parent {
                            Some(p) => {
                                crate::cli::plan_cmd::add(pool, &p, &title, None, None, &[], &agent)
                                    .await
                                    .map(|_| format!("added under {p}"))
                            }
                            None => crate::cli::plan_cmd::create(pool, &title, None, &agent)
                                .await
                                .map(|t| format!("created epic seq={}", t.seq)),
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

        // Dashboard inline-rename overlay captures keys while active.
        // Mirrors the DAG add-mode pattern; Enter commits, Esc cancels,
        // Ctrl-C still quits.
        if self.active_view == ActiveView::Dashboard && self.dashboard.rename_mode() {
            match code {
                KeyCode::Esc => self.dashboard.rename_cancel(),
                KeyCode::Backspace => self.dashboard.rename_pop(),
                KeyCode::Enter => self.dashboard.rename_commit(pool).await,
                KeyCode::Char(c) => {
                    if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                        self.should_quit = true;
                    } else {
                        self.dashboard.rename_push(c);
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

        // Pending delete-confirm takes precedence — whatever the next key is,
        // it either commits (`y`) or cancels. Ctrl-C still quits. Prevents a
        // stray keystroke from firing a tab switch while the prompt is armed.
        match self.active_view {
            ActiveView::Dag if self.dag.pending_delete.is_some() => {
                if matches!(code, KeyCode::Char(c) if c == 'y' || c == 'Y') {
                    if let Some(id) = self.dag.take_pending_delete() {
                        let label = id.to_string()[..8].to_string();
                        match crate::models::task::TaskRepo::new(pool).delete(id).await {
                            Ok(()) => self.dag.flash = format!("deleted {label}"),
                            Err(e) => self.dag.flash = format!("delete failed: {e}"),
                        }
                        let _ = self.dag.refresh(pool).await;
                    }
                } else if matches!(code, KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL))
                {
                    self.should_quit = true;
                } else {
                    self.dag.delete_cancel();
                }
                return;
            }
            ActiveView::Tasks if self.tasks.pending_delete.is_some() => {
                if matches!(code, KeyCode::Char(c) if c == 'y' || c == 'Y') {
                    if let Some(id) = self.tasks.take_pending_delete() {
                        let label = id.to_string()[..8].to_string();
                        match crate::models::task::TaskRepo::new(pool).delete(id).await {
                            Ok(()) => self.tasks.set_flash(format!("deleted {label}")),
                            Err(e) => self.tasks.set_flash(format!("delete failed: {e}")),
                        }
                        let _ = self.tasks.refresh(pool).await;
                    }
                } else if matches!(code, KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL))
                {
                    self.should_quit = true;
                } else {
                    self.tasks.delete_cancel();
                }
                return;
            }
            _ => {}
        }

        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Backspace if self.active_view == ActiveView::Dag => {
                self.dag.delete_begin();
            }
            KeyCode::Backspace if self.active_view == ActiveView::Tasks => {
                self.tasks.delete_begin();
            }
            KeyCode::Char('1') => self.set_view(ActiveView::Dashboard),
            KeyCode::Char('2') => self.set_view(ActiveView::Dag),
            KeyCode::Char('3') => self.set_view(ActiveView::Tasks),
            KeyCode::Char('4') => self.set_view(ActiveView::Trace),
            KeyCode::Char('5') => self.set_view(ActiveView::Query),
            KeyCode::Char('6') => self.set_view(ActiveView::Logs),
            KeyCode::Char('7') => self.set_view(ActiveView::MemGraph),
            KeyCode::Char('8') => self.set_view(ActiveView::Eval),
            KeyCode::Char('9') => self.set_view(ActiveView::Prompt),
            KeyCode::Char('0') => self.set_view(ActiveView::Locks),
            KeyCode::Char('R') => self.set_view(ActiveView::Runs),
            KeyCode::Char('f') if self.active_view == ActiveView::Runs => {
                self.runs.cycle_filter();
                let _ = self.runs.refresh(pool).await;
            }
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
            KeyCode::Char('r') if self.active_view == ActiveView::Tasks => {
                // Run the selected task from Tasks pane. Mirrors DAG 'r'.
                if let Some(task_ref) = self.tasks.selected_task_ref() {
                    let agent = std::env::var("YGG_AGENT_NAME")
                        .ok()
                        .unwrap_or_else(|| self.agent_name.clone());
                    let silent = |_: &str| {};
                    match crate::cli::plan_cmd::run_with_reporter(
                        pool, &task_ref, &agent, false, &silent,
                    )
                    .await
                    {
                        Ok(headline) => self.tasks.set_flash(headline),
                        Err(e) => self.tasks.set_flash(format!("run failed: {e}")),
                    }
                    let _ = self.tasks.refresh(pool).await;
                }
            }
            KeyCode::Char('r') if self.active_view == ActiveView::Dag && !self.dag.add_mode() => {
                if let Some(task_ref) = self.dag.selected_task_ref() {
                    let agent = std::env::var("YGG_AGENT_NAME")
                        .ok()
                        .unwrap_or_else(|| self.agent_name.clone());
                    let silent = |_: &str| {};
                    match crate::cli::plan_cmd::run_with_reporter(
                        pool, &task_ref, &agent, false, &silent,
                    )
                    .await
                    {
                        Ok(headline) => self.dag.flash = headline,
                        Err(e) => self.dag.flash = format!("run failed: {e}"),
                    }
                    let _ = self.dag.refresh(pool).await;
                }
            }
            KeyCode::Char('n') if self.active_view == ActiveView::Dag && !self.dag.add_mode() => {
                self.dag.add_begin();
            }
            KeyCode::Char('S') if self.active_view == ActiveView::Dashboard => {
                self.dashboard.toggle_session_scope();
                let _ = self.dashboard.refresh(pool).await;
            }
            KeyCode::Char('r')
                if self.active_view == ActiveView::Dashboard
                    && self.dashboard.focus == super::dashboard::DashboardFocus::Agents =>
            {
                self.dashboard.rename_begin();
            }
            KeyCode::Char('a')
                if self.active_view == ActiveView::Dashboard
                    && self.dashboard.focus == super::dashboard::DashboardFocus::Agents =>
            {
                self.dashboard.archive_selected(pool).await;
            }
            KeyCode::Char('w') if self.active_view == ActiveView::Dashboard => {
                // Toggle focus between agents + workers panel. Whatever's
                // focused gets arrow-key + Enter attention.
                match self.dashboard.focus {
                    super::dashboard::DashboardFocus::Agents => self.dashboard.workers_focus(),
                    super::dashboard::DashboardFocus::Workers => self.dashboard.agents_focus(),
                }
            }
            KeyCode::Char('w') if self.active_view == ActiveView::Eval => {
                self.eval.cycle_window();
                let _ = self.eval.refresh(pool).await;
            }
            KeyCode::Char('r') if self.active_view == ActiveView::Locks => {
                let cfg = AppConfig::from_env().ok();
                let ttl = cfg.as_ref().map(|c| c.lock_ttl_secs).unwrap_or(300);
                self.locks.release_selected(pool, ttl).await;
            }
            KeyCode::Up => match self.active_view {
                ActiveView::Dag => self.dag.scroll_up(),
                ActiveView::Dashboard => match self.dashboard.focus {
                    super::dashboard::DashboardFocus::Agents => self.dashboard.select_prev(),
                    super::dashboard::DashboardFocus::Workers => self.dashboard.worker_up(),
                },
                ActiveView::Tasks => self.tasks.select_prev(),
                ActiveView::Trace => self.trace.select_prev(),
                ActiveView::Logs => self.logs.scroll_up(),
                ActiveView::MemGraph => self.memgraph.scroll_up(),
                ActiveView::Prompt => self.prompt.select_prev(),
                ActiveView::Locks => self.locks.select_prev(),
                ActiveView::Runs => self.runs.select_prev(),
                _ => {}
            },
            KeyCode::Down => match self.active_view {
                ActiveView::Dag => self.dag.scroll_down(),
                ActiveView::Dashboard => match self.dashboard.focus {
                    super::dashboard::DashboardFocus::Agents => self.dashboard.select_next(),
                    super::dashboard::DashboardFocus::Workers => self.dashboard.worker_down(),
                },
                ActiveView::Tasks => self.tasks.select_next(),
                ActiveView::Trace => self.trace.select_next(),
                ActiveView::Logs => self.logs.scroll_down(),
                ActiveView::MemGraph => self.memgraph.scroll_down(),
                ActiveView::Prompt => self.prompt.select_next(),
                ActiveView::Locks => self.locks.select_next(),
                ActiveView::Runs => self.runs.select_next(),
                _ => {}
            },
            KeyCode::PageUp if self.active_view == ActiveView::Prompt => {
                self.prompt.scroll_up();
            }
            KeyCode::PageDown if self.active_view == ActiveView::Prompt => {
                self.prompt.scroll_down();
            }
            KeyCode::Enter => match self.active_view {
                ActiveView::Dashboard => match self.dashboard.focus {
                    super::dashboard::DashboardFocus::Workers => {
                        // Attach to the selected worker's tmux window.
                        // Actual tmux exec runs after we tear down ratatui
                        // in the outer `run` loop — writing to a pending
                        // slot avoids half-mode-switched terminals.
                        if let Some(w) = self.dashboard.selected_worker().cloned() {
                            self.attach_pending = Some((w.tmux_session, w.tmux_window));
                            self.should_quit = true;
                        }
                    }
                    super::dashboard::DashboardFocus::Agents => {
                        // Jump to DAG with the owner filter pre-set to the
                        // agent whose row was selected.
                        if let Some(agent) = self.dashboard.selected_agent_full().cloned() {
                            self.dag.agent_filter =
                                super::dag_view::AgentFilter::Specific(agent.agent_id);
                            let _ = self.dag.refresh(pool).await;
                            self.set_view(ActiveView::Dag);
                        }
                    }
                },
                ActiveView::Dag => self.dag.toggle_detail(),
                ActiveView::Logs => self.logs.toggle_detail(),
                ActiveView::MemGraph => self.memgraph.toggle_detail(),
                ActiveView::Tasks => self.tasks.toggle_detail(),
                _ => {}
            },
            KeyCode::Esc => match self.active_view {
                ActiveView::Dag if self.dag.detail_open => self.dag.detail_open = false,
                ActiveView::Tasks if self.tasks.detail_open => self.tasks.detail_open = false,
                ActiveView::MemGraph if self.memgraph.detail_open => {
                    self.memgraph.detail_open = false
                }
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
            ActiveView::Eval => ActiveView::Prompt,
            ActiveView::Prompt => ActiveView::Locks,
            ActiveView::Locks => ActiveView::Runs,
            ActiveView::Runs => ActiveView::Dashboard,
        };
        self.set_view(next);
    }
    fn cycle_view_backward(&mut self) {
        let prev = match self.active_view {
            ActiveView::Dashboard => ActiveView::Runs,
            ActiveView::Dag => ActiveView::Dashboard,
            ActiveView::Tasks => ActiveView::Dag,
            ActiveView::Trace => ActiveView::Tasks,
            ActiveView::Query => ActiveView::Trace,
            ActiveView::Logs => ActiveView::Query,
            ActiveView::MemGraph => ActiveView::Logs,
            ActiveView::Eval => ActiveView::MemGraph,
            ActiveView::Prompt => ActiveView::Eval,
            ActiveView::Locks => ActiveView::Prompt,
            ActiveView::Runs => ActiveView::Locks,
        };
        self.set_view(prev);
    }

    /// Paint the whole TUI into `frame`. Extracted so the run loop can call it
    /// both before and after a refresh — painting before refresh lets each
    /// view's own "loading" state be visible while the DB query blocks.
    /// Test-only entry point so integration tests can exercise the layout
    /// against `TestBackend` without spinning up a Postgres connection.
    /// Production code goes through `run` → `draw`.
    #[doc(hidden)]
    pub fn draw_for_test(&mut self, frame: &mut Frame) {
        self.draw(frame)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // yggdrasil-156: narrow-terminal collapse. Below 100 cols the wide
        // `[1] Dashboard [2] DAG …` row wraps and the right-hand ops-stats
        // panel crowds out the event tail. Drop both to compact equivalents.
        let narrow = area.width < NARROW_TERMINAL_THRESHOLD;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // tab row
                Constraint::Length(1), // context-sensitive help row
                Constraint::Min(0),    // active pane
                Constraint::Length(3), // global status strip (3 recent events)
            ])
            .split(area);

        let tab = |label: &str, active: bool| -> Span<'static> {
            if active {
                Span::styled(
                    format!(" {label} "),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                )
            } else {
                Span::styled(format!(" {label} "), Style::default().fg(Color::Gray))
            }
        };

        let tabs: Vec<Span<'static>> = if narrow {
            // Compact tabs: just the activator digit/letter. The full label
            // moves to the help row's pane_hint, which already shows the
            // active view's affordances.
            [
                ("1", ActiveView::Dashboard),
                ("2", ActiveView::Dag),
                ("3", ActiveView::Tasks),
                ("4", ActiveView::Trace),
                ("5", ActiveView::Query),
                ("6", ActiveView::Logs),
                ("7", ActiveView::MemGraph),
                ("8", ActiveView::Eval),
                ("9", ActiveView::Prompt),
                ("0", ActiveView::Locks),
                ("R", ActiveView::Runs),
            ]
            .iter()
            .map(|(k, v)| tab(k, self.active_view == *v))
            .collect()
        } else {
            vec![
                tab("[1] Dashboard", self.active_view == ActiveView::Dashboard),
                tab("[2] DAG", self.active_view == ActiveView::Dag),
                tab("[3] Tasks", self.active_view == ActiveView::Tasks),
                tab("[4] Trace", self.active_view == ActiveView::Trace),
                tab("[5] Query", self.active_view == ActiveView::Query),
                tab("[6] Logs", self.active_view == ActiveView::Logs),
                tab("[7] Memgraph", self.active_view == ActiveView::MemGraph),
                tab("[8] Eval", self.active_view == ActiveView::Eval),
                tab("[9] Prompt", self.active_view == ActiveView::Prompt),
                tab("[0] Locks", self.active_view == ActiveView::Locks),
                tab("[R] Runs", self.active_view == ActiveView::Runs),
            ]
        };
        frame.render_widget(Line::from(tabs), chunks[0]);

        let nav_span = Span::styled(
            "  q=quit  ←→/tab=nav  Enter=detail  ",
            Style::default().fg(Color::DarkGray),
        );
        let pane_hint = match self.active_view {
            ActiveView::Dashboard => "S=session-scope",
            ActiveView::Dag => {
                "Enter=detail  r=run  n=add  ⌫=delete  s=sort  a=agent  f=focus  c=clear"
            }
            ActiveView::Tasks => "↑↓ select  ·  Enter=detail  ·  r=run  ·  ⌫=delete",
            ActiveView::Trace => "↑↓ select",
            ActiveView::Query => "type then Enter  ·  Esc=leave",
            ActiveView::Logs => "f=filter  Enter=detail",
            ActiveView::MemGraph => "↑↓ scroll  Enter=detail  Esc=close",
            ActiveView::Eval => "w=cycle window (1h/6h/24h/7d)",
            ActiveView::Prompt => "↑↓ pins · PgUp/PgDn scroll MEMORY.md",
            ActiveView::Locks => "↑↓ select  ·  r=release",
            ActiveView::Runs => "↑↓ select  ·  f=cycle filter (all/live/terminal)",
        };
        let hint_line = Line::from(vec![
            nav_span,
            Span::styled(
                pane_hint.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]);
        frame.render_widget(hint_line, chunks[1]);

        match self.active_view {
            ActiveView::Dashboard => self.dashboard.render(frame, chunks[2]),
            ActiveView::Dag => self.dag.render(frame, chunks[2]),
            ActiveView::Tasks => self.tasks.render(frame, chunks[2]),
            ActiveView::Trace => self.trace.render(frame, chunks[2]),
            ActiveView::Query => self.query.render(frame, chunks[2]),
            ActiveView::Logs => self.logs.render(frame, chunks[2]),
            ActiveView::MemGraph => self.memgraph.render(frame, chunks[2]),
            ActiveView::Eval => self.eval.render(frame, chunks[2]),
            ActiveView::Prompt => self.prompt.render(frame, chunks[2]),
            ActiveView::Locks => self.locks.render(frame, chunks[2]),
            ActiveView::Runs => self.runs.render(frame, chunks[2]),
        }

        // Global status strip — two columns at wide widths (events left,
        // orchestration stats right); single-column at narrow widths so the
        // 34-col ops panel doesn't crowd out the event tail. yggdrasil-156.
        let strip_constraints: &[Constraint] = if narrow {
            &[Constraint::Min(0)]
        } else {
            &[Constraint::Min(40), Constraint::Length(34)]
        };
        let strip = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(strip_constraints)
            .split(chunks[3]);

        let event_lines: Vec<Line> = self
            .status_tail
            .iter()
            .map(|(ts, kind, detail)| {
                let (glyph, color) = event_glyph(kind);
                Line::from(vec![
                    Span::styled(ts.clone(), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::styled(format!("{kind:<18}"), Style::default().fg(color)),
                    Span::raw(" "),
                    Span::styled(detail.clone(), Style::default().fg(Color::Gray)),
                ])
            })
            .collect();
        let events_panel = ratatui::widgets::Paragraph::new(event_lines).block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::TOP)
                .title(" events "),
        );
        frame.render_widget(events_panel, strip[0]);

        let s = &self.ops_stats;
        let ollama_style = if s.ollama_ok {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };
        let stuck_line = if s.agents_stuck > 0 {
            Line::from(vec![
                Span::styled(
                    "  ⚠ ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{} stuck", s.agents_stuck),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("  ⚡ ollama ", Style::default().fg(Color::DarkGray)),
                Span::styled(if s.ollama_ok { "up" } else { "down" }, ollama_style),
                Span::styled(
                    format!("  db {}ms", s.db_ms),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        };
        // yggdrasil-150: 10-glyph sparkline of recent alive-count history,
        // appended to the "● live" row so liveness trend is one glance.
        let alive_spark = self.alive_history.glyphs(10);
        let stats_lines = vec![
            Line::from(vec![
                Span::styled("  ● ", Style::default().fg(Color::Green)),
                Span::styled(
                    format!("{} live", s.agents_alive),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {alive_spark}"), Style::default().fg(Color::Green)),
                Span::styled(
                    format!(" / {} sessions", s.live_sessions),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(vec![
                Span::styled("  ▶ ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{} tasks running", s.tasks_running),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            stuck_line,
        ];
        if !narrow {
            let stats_panel = ratatui::widgets::Paragraph::new(stats_lines).block(
                ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::TOP)
                    .title(" orchestration "),
            );
            frame.render_widget(stats_panel, strip[1]);
        }
    }
}

/// Width threshold below which the global TUI chrome collapses to a
/// single-column compact form. Picked empirically: 100 cols is the
/// narrowest the wide tab row + 34-col ops panel render without overlap.
pub const NARROW_TERMINAL_THRESHOLD: u16 = 100;

/// Glyph + color for an event kind — mirrors src/cli/logs_cmd.rs::kind_style.
fn event_glyph(kind: &str) -> (&'static str, Color) {
    match kind {
        "node_written" => ("●", Color::Green),
        "lock_acquired" => ("⚿", Color::Yellow),
        "lock_released" => ("○", Color::DarkGray),
        "digest_written" => ("◈", Color::Cyan),
        "similarity_hit" => ("≈", Color::Blue),
        "correction_detected" => ("✗", Color::Red),
        "hook_fired" => ("▸", Color::Yellow),
        "embedding_call" => ("⚡", Color::Cyan),
        "task_created" => ("✚", Color::Green),
        "task_status_changed" => ("◆", Color::Yellow),
        "remembered" => ("♦", Color::Blue),
        "embedding_cache_hit" => ("⚡", Color::Green),
        "classifier_decision" => ("⚖", Color::Cyan),
        "scoring_decision" => ("·", Color::Gray),
        "redaction_applied" => ("✂", Color::Red),
        "hit_referenced" => ("✓", Color::Green),
        "agent_state_changed" => ("↪", Color::Blue),
        _ => ("·", Color::Gray),
    }
}

/// Boot reconciliation. Cross-check the workers table against live tmux
/// windows; abandon any row whose window is gone. Called once at TUI
/// startup and also cheap to call from anywhere else (idempotent).
pub async fn reconcile_workers(pool: &PgPool) -> Result<(), anyhow::Error> {
    use crate::models::worker::{WorkerRepo, WorkerState};
    use std::collections::{HashMap, HashSet};
    let workers = WorkerRepo::new(pool).list_live().await.unwrap_or_default();
    if workers.is_empty() {
        return Ok(());
    }

    let mut by_session: HashMap<String, HashSet<String>> = HashMap::new();
    for w in &workers {
        by_session.entry(w.tmux_session.clone()).or_default();
    }
    for session in by_session.keys().cloned().collect::<Vec<_>>() {
        let out = std::process::Command::new("tmux")
            .args(["list-windows", "-t", &session, "-F", "#{window_name}"])
            .output();
        let set = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .collect(),
            _ => HashSet::new(), // session missing → treat all as gone
        };
        by_session.insert(session, set);
    }

    let repo = WorkerRepo::new(pool);
    let mut n = 0;
    for w in workers {
        let live = by_session
            .get(&w.tmux_session)
            .map(|set| set.contains(&w.tmux_window))
            .unwrap_or(false);
        if !live {
            let _ = repo
                .set_state(
                    w.worker_id,
                    WorkerState::Abandoned,
                    Some("reconciled at TUI start — window absent"),
                )
                .await;
            n += 1;
        }
    }
    if n > 0 {
        tracing::info!(reconciled = n, "worker boot reconciliation");
    }
    Ok(())
}

async fn reqwest_ping() -> bool {
    let base = std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    reqwest::get(url)
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn short_status_detail(p: &serde_json::Value) -> String {
    // One-line detail for the bottom status strip. Best-effort per kind.
    if let Some(score) = p
        .get("total_score")
        .or_else(|| p.get("similarity"))
        .and_then(|v| v.as_f64())
    {
        let src = p
            .get("source_agent")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let snip = p.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
        let s = if snip.chars().count() > 40 {
            snip.chars().take(40).collect::<String>() + "…"
        } else {
            snip.to_string()
        };
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
        } else {
            snip.to_string()
        };
        return s;
    }
    String::new()
}

/// Run the TUI event loop.
pub async fn run(pool: &PgPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let agent_name = std::env::var("YGG_AGENT_NAME").unwrap_or_else(|_| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "ygg".to_string())
    });

    // Boot reconciliation (yggdrasil-53). Any worker row marked live
    // whose tmux window is absent gets flipped to abandoned so the
    // dashboard reflects reality after a machine restart.
    if let Err(e) = reconcile_workers(pool).await {
        tracing::warn!(error = %e, "worker reconciliation on TUI start failed");
    }

    let mut app = App::new(agent_name);

    // Decouple refresh from input. Previously every keypress triggered
    // a full DB+Ollama refresh cascade (~hundreds of ms) before the
    // next draw — arrow keys felt laggy. Now we refresh on a timer
    // (every 2s) while key polling at a short interval stays snappy.
    // Targeted per-action refreshes (e.g. after 'r' runs) still fire
    // from handle_key directly.
    use std::time::Instant;
    let refresh_interval = std::time::Duration::from_secs(2);
    let poll_interval = std::time::Duration::from_millis(50);
    let mut last_refresh: Option<Instant> = None;

    loop {
        // Draw every tick so input stays snappy and each view's own
        // "loading" state is visible until its first refresh lands.
        terminal.draw(|frame| app.draw(frame))?;

        // Refresh on a coarser timer — every keypress refreshing the DB
        // made arrow keys laggy. Targeted per-action refreshes still fire
        // from handle_key directly.
        let need_refresh = last_refresh
            .map(|t| t.elapsed() >= refresh_interval)
            .unwrap_or(true);

        if need_refresh {
            app.dashboard.refresh(pool).await?;
            app.refresh_status_tail(pool).await;
            match app.active_view {
                ActiveView::Dag => {
                    app.dag.refresh(pool).await?;
                }
                ActiveView::Tasks => {
                    app.tasks.refresh(pool).await?;
                }
                ActiveView::Trace => {
                    app.trace.refresh(pool).await?;
                }
                ActiveView::Logs => {
                    app.logs.refresh(pool).await?;
                }
                ActiveView::MemGraph => {
                    app.memgraph.refresh(pool).await?;
                }
                ActiveView::Eval => {
                    app.eval.refresh(pool).await?;
                }
                ActiveView::Prompt => {
                    app.prompt.refresh(pool).await?;
                }
                ActiveView::Locks => {
                    app.locks.refresh(pool, config.lock_ttl_secs).await?;
                }
                ActiveView::Runs => {
                    app.runs.refresh(pool).await?;
                }
                _ => {}
            }
            last_refresh = Some(Instant::now());
        }

        if event::poll(poll_interval)? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(pool, key.code, key.modifiers).await;
            }
        }

        if app.should_quit {
            break;
        }
    }

    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Honor a deferred tmux attach from Workers-panel Enter. If we're
    // already inside tmux, switch-client; otherwise, exec attach so
    // the ygg-tui process is replaced by tmux.
    if let Some((session, window)) = app.attach_pending {
        let target = format!("{session}:{window}");
        if std::env::var("TMUX").is_ok() {
            let _ = std::process::Command::new("tmux")
                .args(["switch-client", "-t", &target])
                .status();
        } else {
            // exec replaces the current process — ratatui is done, so
            // hand the terminal over to tmux cleanly.
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new("tmux")
                .args([
                    "attach",
                    "-t",
                    &session,
                    ";",
                    "select-window",
                    "-t",
                    &target,
                ])
                .exec();
            // exec returns only on failure; print and continue.
            eprintln!("tmux attach failed: {err}");
        }
    }
    Ok(())
}
