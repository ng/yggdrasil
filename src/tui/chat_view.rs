//! Chat panel — real-time message thread view with compose overlay.
//!
//! Shows all recent messages (directed + broadcast) in a scrollable
//! list. Compose with `c` (type `@agent msg` for directed, or just
//! `msg` for broadcast). Claim unclaimed broadcasts with Enter.

use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use sqlx::PgPool;
use uuid::Uuid;

use crate::cli::msg_cmd;

#[derive(Debug, Clone)]
struct ChatMsg {
    id: Uuid,
    from_name: String,
    to_name: Option<String>,
    body: String,
    created_at: DateTime<Utc>,
}

pub struct ChatView {
    messages: Vec<ChatMsg>,
    state: ListState,
    loaded: bool,
    pub compose: Option<ComposeState>,
    pub flash: Option<String>,
}

pub struct ComposeState {
    pub buf: String,
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            state: ListState::default(),
            loaded: false,
            compose: None,
            flash: None,
        }
    }

    pub fn composing(&self) -> bool {
        self.compose.is_some()
    }

    pub fn compose_begin(&mut self) {
        self.compose = Some(ComposeState { buf: String::new() });
    }

    pub fn compose_cancel(&mut self) {
        self.compose = None;
    }

    pub fn compose_push(&mut self, c: char) {
        if let Some(cs) = self.compose.as_mut() {
            cs.buf.push(c);
        }
    }

    pub fn compose_pop(&mut self) {
        if let Some(cs) = self.compose.as_mut() {
            cs.buf.pop();
        }
    }

    /// Parse compose buffer and send. `@agent rest` = directed; plain text = broadcast.
    pub async fn compose_commit(&mut self, pool: &PgPool, from_agent: &str) {
        let Some(cs) = self.compose.take() else {
            return;
        };
        let input = cs.buf.trim().to_string();
        if input.is_empty() {
            self.flash = Some("cancelled (empty)".into());
            return;
        }

        if let Some(rest) = input.strip_prefix('@') {
            if let Some(space) = rest.find(' ') {
                let to = &rest[..space];
                let body = rest[space + 1..].trim();
                if body.is_empty() {
                    self.flash = Some("cancelled (empty body)".into());
                    return;
                }
                match msg_cmd::send(pool, from_agent, to, body, true).await {
                    Ok(()) => self.flash = Some(format!("sent to {to}")),
                    Err(e) => self.flash = Some(format!("send failed: {e}")),
                }
            } else {
                self.flash = Some("usage: @agent message".into());
            }
        } else {
            match msg_cmd::broadcast(pool, from_agent, &input).await {
                Ok(_id) => self.flash = Some("broadcast sent".into()),
                Err(e) => self.flash = Some(format!("broadcast failed: {e}")),
            }
        }
    }

    /// Claim the selected broadcast message.
    pub async fn claim_selected(&mut self, pool: &PgPool, agent_name: &str) {
        let Some(idx) = self.state.selected() else {
            return;
        };
        let Some(msg) = self.messages.get(idx) else {
            return;
        };
        if msg.to_name.is_some() {
            self.flash = Some("not a broadcast — already directed".into());
            return;
        }
        match msg_cmd::claim_broadcast(pool, msg.id, agent_name).await {
            Ok(()) => self.flash = Some(format!("claimed by {agent_name}")),
            Err(e) => self.flash = Some(format!("claim failed: {e}")),
        }
    }

    pub fn select_next(&mut self) {
        if !self.messages.is_empty() {
            let i = self.state.selected().unwrap_or(0);
            self.state
                .select(Some((i + 1).min(self.messages.len() - 1)));
        }
    }

    pub fn select_prev(&mut self) {
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(i.saturating_sub(1)));
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        self.loaded = true;
        let msgs = msg_cmd::all_messages(pool, 24, 200).await?;
        self.messages = msgs
            .into_iter()
            .map(|m| ChatMsg {
                id: m.id,
                from_name: m.from_name,
                to_name: m.to_name,
                body: m.body,
                created_at: m.created_at,
            })
            .collect();
        // all_messages returns newest-first; reverse for chronological display
        self.messages.reverse();

        if self.messages.is_empty() {
            self.state.select(None);
        } else {
            match self.state.selected() {
                Some(cur) if cur < self.messages.len() => {}
                _ => self.state.select(Some(self.messages.len() - 1)),
            }
        }
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.loaded {
            let p = Paragraph::new(" loading messages… ")
                .block(Block::default().borders(Borders::ALL).title(" Chat "));
            frame.render_widget(p, area);
            return;
        }

        if self.messages.is_empty() {
            let hint = if let Some(ref f) = self.flash {
                format!(" {f} ")
            } else {
                " no messages in last 24h · press [c] to compose ".into()
            };
            let p =
                Paragraph::new(hint).block(Block::default().borders(Borders::ALL).title(" Chat "));
            frame.render_widget(p, area);
            if self.compose.is_some() {
                self.render_compose_overlay(frame, area);
            }
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);

        let items: Vec<ListItem> = self
            .messages
            .iter()
            .map(|m| {
                let ts = m.created_at.format("%H:%M");
                let arrow = match &m.to_name {
                    Some(to) => format!("{} → {}", m.from_name, to),
                    None => format!("{} → ALL", m.from_name),
                };
                let body = truncate(&m.body, 60);
                let style = if m.to_name.is_none() {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{ts}  "), Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{arrow:<28}"), style),
                    Span::raw(body),
                ]))
            })
            .collect();

        let title = format!(" Chat · {} msg(s) · 24h ", self.messages.len());
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(list, chunks[0], &mut self.state);

        // Hint bar
        let hint = if let Some(ref f) = self.flash {
            f.clone()
        } else {
            " c=compose  ↑↓=scroll  Enter=claim broadcast".into()
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );

        if self.compose.is_some() {
            self.render_compose_overlay(frame, area);
        }
    }

    fn render_compose_overlay(&self, frame: &mut Frame, area: Rect) {
        let Some(cs) = &self.compose else {
            return;
        };
        let w = area.width.min(60);
        let h = 5u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let popup = Rect::new(x, y, w, h);

        Clear.render(popup, frame.buffer_mut());

        let display = format!("{}█", cs.buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" compose (@agent msg = directed, plain = broadcast) ")
            .border_style(Style::default().fg(Color::Cyan));
        let p = Paragraph::new(display)
            .block(block)
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: false });
        frame.render_widget(p, popup);
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
