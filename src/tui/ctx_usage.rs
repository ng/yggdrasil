//! Live context-window measurement, shared across TUI views.
//!
//! Tokens come from the latest `usage` block in an agent's Claude Code
//! transcript JSONL — the same number CC's own status line shows.
//! The hard cap is detected per-session: 1M when the `context-1m`
//! beta marker appears in the transcript, otherwise 200K.
//!
//! Color/coloring rules use *both* the per-session hard cap and
//! absolute soft knees (200K / 300K) — research consistently puts
//! recall degradation around 250K regardless of the model's hard
//! limit, so a 1M-cap session above 300K still warrants a warning
//! even though it's only 30% of the cap.

use ratatui::style::Color;

/// Hard cap when CC opted into the 1M-context beta.
pub const HARD_CAP_1M: i64 = 1_000_000;
/// Hard cap for the standard Sonnet/Opus/Haiku context window.
pub const HARD_CAP_DEFAULT: i64 = 200_000;
/// Soft knee where general degradation research shows recall starting
/// to slip — render yellow from here even on a 1M-cap session.
pub const SOFT_DEGRADATION: i64 = 200_000;
/// Above this we're well past the soft knee — orange.
pub const SOFT_HARD_WARN: i64 = 300_000;

/// Per-component breakdown of the last `usage` block.
#[derive(Debug, Default, Clone, Copy)]
pub struct UsageBreakdown {
    pub cache_read: i64,
    pub cache_creation: i64,
    pub input: i64,
    pub output: i64,
    pub hard_cap: i64,
}

impl UsageBreakdown {
    pub fn total(&self) -> i64 {
        self.cache_read + self.cache_creation + self.input + self.output
    }
}

/// Live context-window state for one agent: (tokens, hard_cap).
pub fn agent_context_usage(agent_name: &str) -> Option<(i64, i64)> {
    let path = latest_transcript(agent_name)?;
    if let Some(b) = parse_last_usage(&path) {
        return Some((b.total(), b.hard_cap));
    }
    // No usage block: fall back to a coarse byte estimate so the row
    // isn't blank. Default cap — we have no signal either way.
    let bytes = std::fs::metadata(&path)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    Some((bytes / 30, HARD_CAP_DEFAULT))
}

/// Last-usage breakdown for one agent — cache_read / cache_creation /
/// input / output split, plus the detected hard cap.
pub fn agent_usage_breakdown(agent_name: &str) -> Option<UsageBreakdown> {
    parse_last_usage(&latest_transcript(agent_name)?)
}

fn latest_transcript(agent_name: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let projects = std::path::PathBuf::from(&home).join(".claude/projects");
    if !projects.exists() {
        return None;
    }
    let entries = std::fs::read_dir(&projects).ok()?;
    let needle = format!("-{agent_name}");
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if !name.ends_with(&needle) {
            continue;
        }
        let Ok(inner) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for f in inner.flatten() {
            if f.path().extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
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
    best.map(|(_, p)| p)
}

/// Walk the JSONL from end to start looking for the last `usage` object
/// and return its component breakdown. Also notes whether the
/// `context-1m` beta marker appears in the tail — presence promotes
/// the cap to 1M.
fn parse_last_usage(path: &std::path::Path) -> Option<UsageBreakdown> {
    // Read the last 200KB — usage blocks are always near the tail, and
    // we avoid reading multi-MB transcripts in full on every refresh.
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let tail_start = len.saturating_sub(200_000);
    file.seek(SeekFrom::Start(tail_start)).ok()?;
    let mut buf = String::new();
    file.take(200_000).read_to_string(&mut buf).ok()?;

    let hard_cap = if buf.contains("context-1m") {
        HARD_CAP_1M
    } else {
        HARD_CAP_DEFAULT
    };

    for line in buf.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let usage = v.pointer("/message/usage").or_else(|| v.pointer("/usage"));
        let Some(u) = usage else { continue };
        let cr = u
            .get("cache_read_input_tokens")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let cc = u
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let inp = u.get("input_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let out = u.get("output_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        if cr + cc + inp + out > 0 {
            return Some(UsageBreakdown {
                cache_read: cr,
                cache_creation: cc,
                input: inp,
                output: out,
                hard_cap,
            });
        }
    }
    None
}

/// Color ramp for a context-tokens reading. Soft knees at 200K and 300K
/// fire even on 1M-cap sessions — degradation research is independent
/// of the model's hard limit.
pub fn ctx_color(tokens: i64, hard_cap: i64) -> Color {
    let near_hard = (hard_cap as f64 * 0.80) as i64;
    if tokens >= near_hard {
        Color::Red
    } else if tokens >= SOFT_HARD_WARN {
        // Orange — Color::LightRed reads as orange in most palettes.
        Color::LightRed
    } else if tokens >= SOFT_DEGRADATION {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Compact token formatter: 392_000 → "392K", 1_200_000 → "1.2M".
pub fn humanize_tokens(n: i64) -> String {
    let abs = n.unsigned_abs() as f64;
    if abs >= 1_000_000.0 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}
