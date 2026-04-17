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
use super::log_view::LogView;
use super::meter_view::MeterView;
use super::query_view::QueryView;
use super::tasks_view::TasksView;
use super::trace_view::TraceView;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActiveView {
    Dashboard,
    Dag,
    Tasks,
    Trace,
    Meter,
    Query,
    Logs,
}

pub struct App {
    pub active_view: ActiveView,
    pub should_quit: bool,
    pub dashboard: DashboardView,
    pub dag: DagView,
    pub tasks: TasksView,
    pub trace: TraceView,
    pub meter: MeterView,
    pub query: QueryView,
    pub logs: LogView,
    pub agent_name: String,
    pub query_focus: bool, // true = typing in Query pane; blocks global keys
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
            meter: MeterView::new(),
            query: QueryView::new(),
            logs: LogView::new(),
            agent_name,
            query_focus: false,
        }
    }

    pub async fn handle_key(&mut self, pool: &PgPool, code: KeyCode, modifiers: KeyModifiers) {
        // When the Query pane has focus for typing, most keys become input.
        if self.query_focus {
            match code {
                KeyCode::Esc => self.query_focus = false,
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
            KeyCode::Char('1') => self.active_view = ActiveView::Dashboard,
            KeyCode::Char('2') => self.active_view = ActiveView::Dag,
            KeyCode::Char('3') => self.active_view = ActiveView::Tasks,
            KeyCode::Char('4') => self.active_view = ActiveView::Trace,
            KeyCode::Char('5') => self.active_view = ActiveView::Meter,
            KeyCode::Char('6') => {
                self.active_view = ActiveView::Query;
                self.query_focus = true;
            }
            KeyCode::Char('7') => self.active_view = ActiveView::Logs,
            KeyCode::Tab => self.cycle_view(),
            KeyCode::Char('i') if self.active_view == ActiveView::Query => {
                self.query_focus = true;
            }
            KeyCode::Char('f') if self.active_view == ActiveView::Logs => {
                self.logs.cycle_filter();
            }
            KeyCode::Up => match self.active_view {
                ActiveView::Dag => self.dag.scroll_up(),
                ActiveView::Dashboard => self.dashboard.select_prev(),
                ActiveView::Tasks => self.tasks.select_prev(),
                ActiveView::Trace => self.trace.select_prev(),
                ActiveView::Logs => self.logs.scroll_up(),
                _ => {}
            },
            KeyCode::Down => match self.active_view {
                ActiveView::Dag => self.dag.scroll_down(),
                ActiveView::Dashboard => self.dashboard.select_next(),
                ActiveView::Tasks => self.tasks.select_next(),
                ActiveView::Trace => self.trace.select_next(),
                ActiveView::Logs => self.logs.scroll_down(),
                _ => {}
            },
            KeyCode::Enter => {
                if self.active_view == ActiveView::Dashboard {
                    if let Some(agent_name) = self.dashboard.selected_agent() {
                        self.dag.set_agent(agent_name);
                        self.active_view = ActiveView::Dag;
                    }
                }
            }
            _ => {}
        }
    }

    fn cycle_view(&mut self) {
        self.active_view = match self.active_view {
            ActiveView::Dashboard => ActiveView::Dag,
            ActiveView::Dag => ActiveView::Tasks,
            ActiveView::Tasks => ActiveView::Trace,
            ActiveView::Trace => ActiveView::Meter,
            ActiveView::Meter => ActiveView::Query,
            ActiveView::Query => ActiveView::Logs,
            ActiveView::Logs => ActiveView::Dashboard,
        };
    }
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
        // Always refresh dashboard (cheap); refresh the active view too.
        app.dashboard.refresh(pool).await?;
        match app.active_view {
            ActiveView::Dag     => { app.dag.refresh(pool).await?; }
            ActiveView::Tasks   => { app.tasks.refresh(pool).await?; }
            ActiveView::Trace   => { app.trace.refresh(pool).await?; }
            ActiveView::Meter   => { app.meter.refresh(pool, &app.agent_name).await?; }
            ActiveView::Logs    => { app.logs.refresh(pool).await?; }
            _ => {}
        }

        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(0)])
                .split(area);

            let tab = |label: &str, active: bool| -> Span<'static> {
                if active {
                    Span::styled(format!(" {label} "),
                        Style::default().fg(Color::Black).bg(Color::Cyan))
                } else {
                    Span::styled(format!(" {label} "), Style::default().fg(Color::Gray))
                }
            };

            let titles = vec![
                tab("[1] Dashboard", app.active_view == ActiveView::Dashboard),
                tab("[2] DAG",       app.active_view == ActiveView::Dag),
                tab("[3] Tasks",     app.active_view == ActiveView::Tasks),
                tab("[4] Trace",     app.active_view == ActiveView::Trace),
                tab("[5] Meter",     app.active_view == ActiveView::Meter),
                tab("[6] Query",     app.active_view == ActiveView::Query),
                tab("[7] Logs",      app.active_view == ActiveView::Logs),
                Span::raw("  q=quit  tab=next  f=filter(logs)  i=input(query)"),
            ];
            frame.render_widget(Line::from(titles), chunks[0]);

            match app.active_view {
                ActiveView::Dashboard => app.dashboard.render(frame, chunks[1]),
                ActiveView::Dag       => app.dag.render(frame, chunks[1]),
                ActiveView::Tasks     => app.tasks.render(frame, chunks[1]),
                ActiveView::Trace     => app.trace.render(frame, chunks[1]),
                ActiveView::Meter     => app.meter.render(frame, chunks[1]),
                ActiveView::Query     => app.query.render(frame, chunks[1]),
                ActiveView::Logs      => app.logs.render(frame, chunks[1]),
            }
        })?;

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
