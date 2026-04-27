//! Smooth gauge widget (yggdrasil-168). btop's signature: 1/8-block
//! partial-fill chars (`▏▎▍▌▋▊▉█`) crossed with a perceptual gradient
//! palette to give 8 sub-cell steps per column × 8 colors = 64
//! effective resolution per cell on a 256-color terminal, more on
//! truecolor.
//!
//! Use cases:
//!   - context-window fill per agent
//!   - run-rate vs cap
//!   - cost vs daily budget
//!   - any 0..=100 % gauge that wants to feel like btop, not Airflow.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

/// 1/8-block partial-fill characters. Index 0 = empty, 8 = full.
/// `LEVELS[n]` covers the open interval `((n-1)/8, n/8]`.
pub const PARTIAL_BLOCKS: &[char] = &[' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Pick the 1/8-block character for a fractional fill in `[0.0, 1.0]`.
/// Empty fill → space; complete fill → full block; everything else
/// rounds to the nearest 1/8 increment.
pub fn partial_block(fraction: f64) -> char {
    let clamped = fraction.clamp(0.0, 1.0);
    let idx = (clamped * 8.0).round() as usize;
    PARTIAL_BLOCKS[idx.min(8)]
}

/// Color palette presets. Viridis is the perceptually-uniform default
/// (low-stimulus → high-stimulus reads as cool → hot consistently).
/// Magma is the "alarmy" variant for cost / pressure gauges where high
/// values are bad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GaugePalette {
    Viridis,
    Magma,
    /// Raw stoplight — green / yellow / red bands at thirds. Cheaper
    /// to read at a glance for boolean-ish "healthy / warn / danger"
    /// gauges; less information-rich than the gradient palettes.
    Stoplight,
}

impl GaugePalette {
    /// Map a fill fraction (0.0–1.0) to an ANSI 256-cube color. The
    /// `Color::Indexed` route works on every terminal that does
    /// 256-color; truecolor fans can drop in `Color::Rgb` later
    /// without reshaping the API.
    pub fn color_at(self, fraction: f64) -> Color {
        let f = fraction.clamp(0.0, 1.0);
        match self {
            // Viridis stops sampled from matplotlib's LUT and quantized
            // to ANSI-256 cube indices. Eight stops; bilinear isn't
            // worth the bytes for a 1-cell-tall gauge.
            Self::Viridis => match (f * 8.0) as usize {
                0 => Color::Indexed(54), // dark purple
                1 => Color::Indexed(60),
                2 => Color::Indexed(67),
                3 => Color::Indexed(73),  // teal
                4 => Color::Indexed(108), // green-teal
                5 => Color::Indexed(149), // green
                6 => Color::Indexed(185), // yellow-green
                _ => Color::Indexed(220), // yellow
            },
            Self::Magma => match (f * 8.0) as usize {
                0 => Color::Indexed(53), // very dark
                1 => Color::Indexed(89),
                2 => Color::Indexed(125), // magenta
                3 => Color::Indexed(161),
                4 => Color::Indexed(197), // pink-red
                5 => Color::Indexed(203),
                6 => Color::Indexed(209), // orange
                _ => Color::Indexed(220), // yellow
            },
            Self::Stoplight => {
                if f < 0.34 {
                    Color::Green
                } else if f < 0.67 {
                    Color::Yellow
                } else {
                    Color::Red
                }
            }
        }
    }
}

/// Smooth gauge that fills a single horizontal row with partial blocks
/// and a colored foreground. Anything taller than one row gets the
/// gauge replicated; the widget is still legible at heights >1 but the
/// design intent is one tight row inside a Block or table cell.
#[derive(Debug, Clone)]
pub struct SmoothGauge {
    pub fraction: f64,
    pub palette: GaugePalette,
    pub label: Option<String>,
}

impl SmoothGauge {
    pub fn new(fraction: f64) -> Self {
        Self {
            fraction: fraction.clamp(0.0, 1.0),
            palette: GaugePalette::Viridis,
            label: None,
        }
    }

    pub fn palette(mut self, palette: GaugePalette) -> Self {
        self.palette = palette;
        self
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

impl Widget for SmoothGauge {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let total_width = area.width as usize;
        // How many full blocks plus the fractional tail block.
        let full_units = self.fraction * total_width as f64;
        let full_cells = full_units.floor() as usize;
        let tail_fraction = full_units - full_cells as f64;

        let color = self.palette.color_at(self.fraction);
        let style = Style::default().fg(color);

        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &mut buf[(area.x + x, area.y + y)];
                let i = x as usize;
                let glyph = if i < full_cells {
                    '█'
                } else if i == full_cells {
                    partial_block(tail_fraction)
                } else {
                    ' '
                };
                cell.set_char(glyph);
                cell.set_style(style);
            }
        }

        // Label drawn dim over the bar's right side; clipped if the bar
        // is too narrow to fit. Left aligned at column 1 so the gauge
        // reads bar-then-label rather than label-on-top-of-bar.
        if let Some(label) = &self.label {
            let label_x = area.x + 1;
            let label_y = area.y;
            let max = area.width.saturating_sub(2) as usize;
            let trimmed: String = label.chars().take(max).collect();
            for (i, c) in trimmed.chars().enumerate() {
                let cell = &mut buf[(label_x + i as u16, label_y)];
                cell.set_char(c);
                cell.set_style(
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
    }
}
