//! Prompt Inspector — shows what gets injected into Claude Code's context
//! on every turn. Three sections:
//!
//!   1. Pinned memories from the `memories` table. These ride along on every
//!      UserPromptSubmit via the hook regardless of semantic similarity —
//!      they're the "always in attention" rules.
//!
//!   2. Scoped learnings. Deterministic (repo, file_glob, rule_id) lookup
//!      on the `learnings` table. Complement to pinned memories — where
//!      pins are global-ish directives, learnings are "when you touch THIS
//!      file, remember X." See ADR 0015.
//!
//!   3. Claude's own auto-memory (`MEMORY.md` + linked files). Loaded by the
//!      harness into the system prompt at session start. Attention to these
//!      decays as context grows, which is why the hook-injected pins above
//!      exist as a complement.

use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use sqlx::PgPool;

use crate::models::learning::{Learning, LearningRepo};
use crate::models::memory::{Memory, MemoryRepo};

pub struct PromptView {
    pub pins: Vec<Memory>,
    pub pin_state: ListState,
    pub learnings: Vec<Learning>,
    pub memory_md: String,
    pub memory_md_path: String,
    pub scroll: u16,
    pub loaded: bool,
}

impl PromptView {
    pub fn new() -> Self {
        let mut st = ListState::default();
        st.select(Some(0));
        Self {
            pins: Vec::new(),
            pin_state: st,
            learnings: Vec::new(),
            memory_md: String::new(),
            memory_md_path: String::new(),
            scroll: 0,
            loaded: false,
        }
    }

    pub fn scroll_up(&mut self) { self.scroll = self.scroll.saturating_sub(3); }
    pub fn scroll_down(&mut self) { self.scroll = self.scroll.saturating_add(3); }

    pub fn select_prev(&mut self) {
        if self.pins.is_empty() { return; }
        let i = self.pin_state.selected().unwrap_or(0);
        self.pin_state.select(Some(if i == 0 { self.pins.len() - 1 } else { i - 1 }));
    }
    pub fn select_next(&mut self) {
        if self.pins.is_empty() { return; }
        let i = self.pin_state.selected().unwrap_or(0);
        self.pin_state.select(Some((i + 1) % self.pins.len()));
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        self.loaded = true;
        self.pins = MemoryRepo::new(pool).list_all_pinned().await.unwrap_or_default();
        // Learnings: show every row (scope filtering happens at hook time,
        // not here — the inspector is a "see everything" surface).
        self.learnings = LearningRepo::new(pool)
            .list_matching(None, None, None).await.unwrap_or_default();
        let (path, contents) = load_memory_md();
        self.memory_md_path = path;
        self.memory_md = contents;
        if self.pin_state.selected().unwrap_or(0) >= self.pins.len() && !self.pins.is_empty() {
            self.pin_state.select(Some(0));
        }
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.loaded {
            let para = Paragraph::new("Loading prompt context…")
                .block(Block::default().borders(Borders::ALL).title(" Prompt inspector "))
                .alignment(Alignment::Center);
            frame.render_widget(para, area);
            return;
        }

        // Three vertical sections: pins · learnings · MEMORY.md. Weights
        // biased to MEMORY.md since it's usually the longest prose; pins
        // and learnings are typically 1-liner rules.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Percentage(30),
                Constraint::Percentage(40),
            ])
            .split(area);

        self.render_pins(frame, chunks[0]);
        self.render_learnings(frame, chunks[1]);
        self.render_memory_md(frame, chunks[2]);
    }

    fn render_pins(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!(" Pinned memories ({}) — always injected via hook, scope-visible ",
            self.pins.len());

        if self.pins.is_empty() {
            let lines = vec![
                Line::from(""),
                Line::from("  No pinned memories."),
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Create one with "),
                    Span::styled("ygg memory create --scope global \"…\"",
                        Style::default().fg(Color::Cyan)),
                    Span::raw(", then "),
                    Span::styled("ygg memory pin <id>",
                        Style::default().fg(Color::Cyan)),
                    Span::raw("."),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Pinned memories ride every UserPromptSubmit hook",
                        Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled("  so they stay in attention as context grows.",
                        Style::default().fg(Color::DarkGray)),
                ]),
            ];
            let para = Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        let items: Vec<ListItem> = self.pins.iter().map(|m| {
            let (scope_color, scope_glyph) = match m.scope {
                crate::models::memory::MemoryScope::Global  => (Color::Magenta, "◉"),
                crate::models::memory::MemoryScope::Repo    => (Color::Cyan,    "▣"),
                crate::models::memory::MemoryScope::Session => (Color::Yellow,  "◆"),
            };
            let age = humanize_age(m.created_at);
            let id_short = &m.memory_id.to_string()[..8];
            let snippet_max = area.width.saturating_sub(40).max(20) as usize;
            let snippet = if m.text.chars().count() > snippet_max {
                m.text.chars().take(snippet_max).collect::<String>() + "…"
            } else { m.text.clone() };

            ListItem::new(Line::from(vec![
                Span::styled("  ★ ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{scope_glyph} {:<7}", m.scope.as_str()),
                    Style::default().fg(scope_color)),
                Span::styled(format!(" {id_short}"), Style::default().fg(Color::DarkGray)),
                Span::styled(format!(" {age:>4}  "), Style::default().fg(Color::DarkGray)),
                Span::raw(snippet),
            ]))
        }).collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
        frame.render_stateful_widget(list, area, &mut self.pin_state);
    }

    fn render_learnings(&self, frame: &mut Frame, area: Rect) {
        let applied_sum: i32 = self.learnings.iter().map(|l| l.applied_count).sum();
        let title = format!(" Learnings ({}) — deterministic scope-match  ·  {} applications ",
            self.learnings.len(), applied_sum);

        if self.learnings.is_empty() {
            let lines = vec![
                Line::from(""),
                Line::from("  No learnings captured yet."),
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Create one with "),
                    Span::styled("ygg learn create \"…\" --file-glob \"src/*.rs\" --rule-id …",
                        Style::default().fg(Color::Cyan)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Scope tuple (repo, file_glob, rule_id) → text.",
                        Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled("  Surfaces when a future task matches the scope.",
                        Style::default().fg(Color::DarkGray)),
                ]),
            ];
            let para = Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        let snippet_max = area.width.saturating_sub(44).max(20) as usize;
        let items: Vec<ListItem> = self.learnings.iter().map(|l| {
            let scope = match (&l.repo_id, &l.file_glob, &l.rule_id) {
                (None, _, _)                   => "global".to_string(),
                (Some(_), Some(g), Some(id))   => format!("{g} · {id}"),
                (Some(_), Some(g), None)       => g.to_string(),
                (Some(_), None,    Some(id))   => format!("rule={id}"),
                (Some(_), None,    None)       => "repo".to_string(),
            };
            let scope_color = if l.repo_id.is_none() { Color::Magenta } else { Color::Cyan };
            let applied = if l.applied_count > 0 {
                format!(" ×{}", l.applied_count)
            } else { String::new() };
            let age = humanize_age(l.created_at);
            let snippet = if l.text.chars().count() > snippet_max {
                l.text.chars().take(snippet_max).collect::<String>() + "…"
            } else { l.text.clone() };

            ListItem::new(Line::from(vec![
                Span::styled("  ◆ ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{scope:<24}"), Style::default().fg(scope_color)),
                Span::styled(format!(" {age:>4}"), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{applied:<5}"),
                    if l.applied_count > 0 {
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    }),
                Span::raw("  "),
                Span::raw(snippet),
            ]))
        }).collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(list, area);
    }

    fn render_memory_md(&self, frame: &mut Frame, area: Rect) {
        let title = if self.memory_md.is_empty() {
            " Claude auto-memory — MEMORY.md (none) ".to_string()
        } else {
            format!(" Claude auto-memory — {} ", self.memory_md_path)
        };

        let body: Vec<Line> = if self.memory_md.is_empty() {
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw("  No "),
                    Span::styled("MEMORY.md", Style::default().fg(Color::Cyan)),
                    Span::raw(" found at:"),
                ]),
                Line::from(vec![
                    Span::styled(format!("    {}", self.memory_md_path),
                        Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Claude Code's harness writes this on its own; it loads",
                        Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled("  into the system prompt at session start.",
                        Style::default().fg(Color::DarkGray)),
                ]),
            ]
        } else {
            self.memory_md.lines().map(|l| Line::from(l.to_string())).collect()
        };

        let para = Paragraph::new(body)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        frame.render_widget(para, area);
    }
}

/// Locate this project's MEMORY.md — `~/.claude/projects/<munged-cwd>/memory/MEMORY.md`.
/// Returns (displayed_path, contents). Contents is empty if the file doesn't exist.
fn load_memory_md() -> (String, String) {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return (String::new(), String::new()),
    };
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return (cwd.to_string_lossy().to_string(), String::new()),
    };
    // Claude munges cwd paths to project-dir names by replacing `/` → `-`.
    let munged = cwd.to_string_lossy().replace('/', "-");
    let path = format!("{home}/.claude/projects/{munged}/memory/MEMORY.md");
    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    (path, contents)
}

fn humanize_age(ts: DateTime<Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0);
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else if secs < 86400 { format!("{}h", secs / 3600) }
    else { format!("{}d", secs / 86400) }
}
