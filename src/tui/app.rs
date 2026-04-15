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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActiveView {
    Dashboard,
    Dag,
}

pub struct App {
    pub active_view: ActiveView,
    pub should_quit: bool,
    pub dashboard: DashboardView,
    pub dag: DagView,
}

impl App {
    pub fn new() -> Self {
        Self {
            active_view: ActiveView::Dashboard,
            should_quit: false,
            dashboard: DashboardView::new(),
            dag: DagView::new(),
        }
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Tab => {
                self.active_view = match self.active_view {
                    ActiveView::Dashboard => ActiveView::Dag,
                    ActiveView::Dag => ActiveView::Dashboard,
                };
            }
            KeyCode::Char('1') => self.active_view = ActiveView::Dashboard,
            KeyCode::Char('2') => self.active_view = ActiveView::Dag,
            KeyCode::Up => {
                if self.active_view == ActiveView::Dag {
                    self.dag.scroll_up();
                } else {
                    self.dashboard.select_prev();
                }
            }
            KeyCode::Down => {
                if self.active_view == ActiveView::Dag {
                    self.dag.scroll_down();
                } else {
                    self.dashboard.select_next();
                }
            }
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
}

/// Run the TUI event loop.
pub async fn run(pool: &PgPool, _config: &AppConfig) -> Result<(), anyhow::Error> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    loop {
        // Refresh data from DB
        app.dashboard.refresh(pool).await?;
        if app.active_view == ActiveView::Dag {
            app.dag.refresh(pool).await?;
        }

        // Draw
        terminal.draw(|frame| {
            let area = frame.area();

            // Tab bar at top
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(0)])
                .split(area);

            let tab_titles = vec![
                Span::styled(
                    " [1] Dashboard ",
                    if app.active_view == ActiveView::Dashboard {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Span::styled(
                    " [2] DAG ",
                    if app.active_view == ActiveView::Dag {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Span::raw("  q=quit  tab=switch  ↑↓=navigate  enter=select"),
            ];
            let tabs = Line::from(tab_titles);
            frame.render_widget(tabs, chunks[0]);

            match app.active_view {
                ActiveView::Dashboard => app.dashboard.render(frame, chunks[1]),
                ActiveView::Dag => app.dag.render(frame, chunks[1]),
            }
        })?;

        // Poll for events with 500ms timeout
        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key.code, key.modifiers);
            }
        }

        if app.should_quit {
            break;
        }
    }

    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
