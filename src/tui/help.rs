//! Context-sensitive help overlay (yggdrasil-132). `?` opens a centered
//! popup listing every key the current pane responds to, plus the
//! global navigation keys. Esc closes. The keymap lives here as plain
//! data so future pane authors update one place.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

/// One key + its effect. The pane name groups bindings by section.
#[derive(Debug, Clone)]
pub struct KeyHint {
    pub keys: &'static str,
    pub effect: &'static str,
}

/// Built-in global keymap rendered above every pane-specific section.
pub const GLOBAL_KEYS: &[KeyHint] = &[
    KeyHint {
        keys: "1..0, R, G, N",
        effect: "switch view",
    },
    KeyHint {
        keys: "Tab / ←→",
        effect: "cycle view forward / backward",
    },
    KeyHint {
        keys: "?",
        effect: "this overlay",
    },
    KeyHint {
        keys: "q, ctrl-c",
        effect: "quit",
    },
    KeyHint {
        keys: "Esc",
        effect: "close overlay / cancel input",
    },
];

/// Per-pane bindings. The active view's slice gets concatenated with
/// `GLOBAL_KEYS` when the overlay opens, so each pane only declares
/// its own keys (no duplication of global ones).
pub fn pane_keys(active: &str) -> &'static [KeyHint] {
    match active {
        "Dashboard" => &[
            KeyHint {
                keys: "↑↓",
                effect: "select agent",
            },
            KeyHint {
                keys: "Enter",
                effect: "drill into selected agent (workers pane)",
            },
            KeyHint {
                keys: "S",
                effect: "toggle session-scope (yggdrasil-134)",
            },
            KeyHint {
                keys: "m",
                effect: "send message to selected agent",
            },
        ],
        "Dag" => &[
            KeyHint {
                keys: "↑↓",
                effect: "scroll",
            },
            KeyHint {
                keys: "Enter",
                effect: "show task detail",
            },
            KeyHint {
                keys: "r",
                effect: "run selected task",
            },
            KeyHint {
                keys: "n",
                effect: "add task",
            },
            KeyHint {
                keys: "⌫",
                effect: "delete (armed; y to confirm)",
            },
            KeyHint {
                keys: "s",
                effect: "cycle sort",
            },
            KeyHint {
                keys: "a",
                effect: "filter by agent",
            },
            KeyHint {
                keys: "f",
                effect: "focus subtree",
            },
            KeyHint {
                keys: "c",
                effect: "clear filters",
            },
        ],
        "Tasks" => &[
            KeyHint {
                keys: "↑↓",
                effect: "select",
            },
            KeyHint {
                keys: "Enter",
                effect: "open detail",
            },
            KeyHint {
                keys: "d",
                effect: "floating overlay (yggdrasil-151)",
            },
            KeyHint {
                keys: "e",
                effect: "rename in place (yggdrasil-155)",
            },
            KeyHint {
                keys: "r",
                effect: "run task",
            },
            KeyHint {
                keys: "⌫",
                effect: "delete (armed; y to confirm)",
            },
        ],
        "Trace" => &[KeyHint {
            keys: "↑↓",
            effect: "select node",
        }],
        "Query" => &[
            KeyHint {
                keys: "type then Enter",
                effect: "run similarity query",
            },
            KeyHint {
                keys: "↑↓",
                effect: "browse hits",
            },
            KeyHint {
                keys: "Esc",
                effect: "leave input mode",
            },
        ],
        "Logs" => &[
            KeyHint {
                keys: "f",
                effect: "cycle filter",
            },
            KeyHint {
                keys: "Enter",
                effect: "show event detail",
            },
        ],
        "MemGraph" => &[
            KeyHint {
                keys: "↑↓",
                effect: "scroll",
            },
            KeyHint {
                keys: "Enter",
                effect: "show node detail",
            },
        ],
        "Eval" => &[KeyHint {
            keys: "w",
            effect: "cycle window (1h / 6h / 24h / 7d)",
        }],
        "Prompt" => &[
            KeyHint {
                keys: "↑↓",
                effect: "select pin",
            },
            KeyHint {
                keys: "PgUp/PgDn",
                effect: "scroll MEMORY.md",
            },
        ],
        "Locks" => &[
            KeyHint {
                keys: "↑↓",
                effect: "select",
            },
            KeyHint {
                keys: "r",
                effect: "release",
            },
        ],
        "Runs" => &[
            KeyHint {
                keys: "↑↓",
                effect: "select",
            },
            KeyHint {
                keys: "f",
                effect: "cycle filter (all / live / terminal)",
            },
        ],
        "RunGrid" => &[
            KeyHint {
                keys: "↑↓",
                effect: "select task row",
            },
            KeyHint {
                keys: "rows",
                effect: "tasks",
            },
            KeyHint {
                keys: "cols",
                effect: "recent attempts (newest left)",
            },
        ],
        "Nerdy" => &[KeyHint {
            keys: "(read-only)",
            effect: "pool / tables / pgvector / hooks deep-dive",
        }],
        _ => &[],
    }
}

/// State carried in App so the overlay survives across refresh ticks.
#[derive(Debug, Default, Clone, Copy)]
pub struct HelpOverlay {
    pub open: bool,
}

impl HelpOverlay {
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }
    pub fn close(&mut self) {
        self.open = false;
    }
}

/// Render the help overlay over `area`, dimming the underlying pane via
/// `Clear` and listing global + pane-specific keys.
pub fn render(area: Rect, buf: &mut Buffer, active_view: &str) {
    let outer = centered_rect(area, 70, 80);
    Clear.render(outer, buf);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Global",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    for k in GLOBAL_KEYS {
        lines.push(format_key_hint(k));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("Pane: {active_view}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    let pane = pane_keys(active_view);
    if pane.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no pane-specific bindings)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for k in pane {
            lines.push(format_key_hint(k));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .title_bottom(Line::from(vec![Span::styled(
            "  ? toggle  ·  Esc close  ",
            Style::default().fg(Color::DarkGray),
        )]));
    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .render(outer, buf);
}

fn format_key_hint(k: &KeyHint) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<14}", k.keys),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(k.effect.to_string(), Style::default().fg(Color::Gray)),
    ])
}

fn centered_rect(outer: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let h = outer.height.saturating_mul(pct_y) / 100;
    let w = outer.width.saturating_mul(pct_x) / 100;
    let x = outer.x + outer.width.saturating_sub(w) / 2;
    let y = outer.y + outer.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}
