//! `ygg bar` — status-bar line generator. Reads Claude Code's statusline
//! JSON from stdin (`{session_id, cost: {total_cost_usd}, context_window:
//! {used_percentage}, ...}`), joins it with Yggdrasil's per-agent state and
//! today's cache/inference savings, and emits a single colored line.
//!
//! Goals (per user feedback):
//!   - drop "idle" — the agent-workflow state isn't meaningful to humans
//!   - round cost to 2 decimal places
//!   - show token usage
//!   - surface cache hits / inference calls saved (the "did this help me?"
//!     signal that's otherwise invisible)
//!
//! Designed to be fast: one DB query, opened per invocation. Refreshes
//! every 3s (Claude Code statusLine default), so keep it under ~100ms.

use chrono::{Duration, Utc};
use std::io::Read;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[38;5;114m";
const YELL: &str = "\x1b[38;5;221m";
const BOLD: &str = "\x1b[1m";

pub async fn execute(pool: &sqlx::PgPool) -> Result<(), anyhow::Error> {
    // Read Claude Code's JSON payload from stdin. Non-fatal if absent.
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    let j: serde_json::Value =
        serde_json::from_str(&input).unwrap_or_else(|_| serde_json::Value::Null);

    let cost_usd = j
        .pointer("/cost/total_cost_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let model_label = j
        .pointer("/model/display_name")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            j.pointer("/model/id")
                .and_then(|v| v.as_str())
                .map(String::from)
        });
    // Effort level lives in ~/.claude/settings.json (e.g. "xhigh"); CC's
    // statusLine JSON doesn't reliably expose it. Read once per refresh —
    // small file, infrequent invocation.
    let effort = read_effort_level();
    // Claude Code's statusLine JSON doesn't reliably expose token_usage
    // under one path. Try several known spellings, then fall back to a
    // transcript-byte-size estimate (~4 chars per token). This way tokens
    // show up next to cost regardless of how CC labels the field.
    let tok_total: i64 = token_count(&j).unwrap_or(0);

    // When CC provides session_id, scope numbers to THIS session (the true
    // "what am I getting out of ygg right now" signal). Otherwise fall back
    // to a 24h global window like before.
    let session_id = j
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let now_minus_24 = Utc::now() - Duration::hours(24);
    let now_minus_48 = Utc::now() - Duration::hours(48);
    // Embedding cache numbers still flow from non-retrieval paths
    // (task dupe detection, occasional manual searches), so the cache
    // hit ratio remains a meaningful signal post-ADR-0015 Phase 1.
    // The retrieval-only signals (similarity_hit, hit_referenced) are
    // gone — see the segment-rendering section below.
    let (cache_hits, embed_calls, cache_prev, calls_prev) = if let Some(sid) = session_id.as_deref()
    {
        // Session-scoped: current session vs last 24h global for trend comparison.
        let row: (i64, i64, i64, i64) = sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit' AND cc_session_id = $1),
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'      AND cc_session_id = $1),
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit' AND created_at >= $2 AND cc_session_id IS DISTINCT FROM $1),
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'      AND created_at >= $2 AND cc_session_id IS DISTINCT FROM $1)
                   FROM events"#,
            )
            .bind(sid)
            .bind(now_minus_24)
            .fetch_one(pool).await.unwrap_or((0, 0, 0, 0));
        row
    } else {
        let row: (i64, i64, i64, i64) = sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit' AND created_at >= $1),
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'      AND created_at >= $1),
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit' AND created_at < $1 AND created_at >= $2),
                     COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'      AND created_at < $1 AND created_at >= $2)
                   FROM events"#,
            )
            .bind(now_minus_24).bind(now_minus_48)
            .fetch_one(pool).await.unwrap_or((0, 0, 0, 0));
        row
    };

    let mut segments: Vec<String> = Vec::new();

    // Context + tokens are the same dimension (how full is the window), so
    // merge them into one segment. Color climbs neutral → yellow → red as
    // pressure rises. Use absolute knees, not percent-of-cap — the cap
    // detection upstream isn't 100% reliable, and degradation research is
    // independent of the model's hard limit. Matches dashboard ctx_color.
    let red = "\x1b[38;5;203m";
    let orange = "\x1b[38;5;208m";
    let (bar_color, value_style) = if tok_total >= 500_000 {
        (red, format!("{red}{BOLD}"))
    } else if tok_total >= 300_000 {
        (orange, format!("{orange}{BOLD}"))
    } else if tok_total >= 200_000 {
        (YELL, format!("{YELL}{BOLD}"))
    } else {
        (GREEN, format!("{BOLD}"))
    };
    let ctx_value = if tok_total > 0 {
        format_tokens(tok_total)
    } else {
        "—".to_string()
    };
    segments.push(format!(
        "{bar_color}▊{RESET} {DIM}ctx{RESET} {value_style}{ctx_value}{RESET}"
    ));

    // Model + effort: low-noise context for "what am I running right now."
    if let Some(m) = model_label {
        let suffix = effort
            .as_deref()
            .map(|e| format!(" {DIM}{e}{RESET}"))
            .unwrap_or_default();
        segments.push(format!("{CYAN}{m}{RESET}{suffix}"));
    } else if let Some(e) = effort.as_deref() {
        segments.push(format!("{DIM}{e}{RESET}"));
    }

    // Session cost — 2dp. Label as "spend" so it reads as verb-action.
    if cost_usd > 0.0 {
        segments.push(format!("{DIM}spend{RESET} {BOLD}${:.2}{RESET}", cost_usd));
    }

    // Yggdrasil metrics — label explicitly with the "ygg" prefix so it's
    // obvious these are our numbers, not Claude Code's. 24h window on both
    // because we don't yet plumb session_id through events — see
    // yggdrasil-26 for the per-session upgrade.
    let cache_total = cache_hits + embed_calls;
    if cache_total > 0 {
        let rate_now = cache_hits as f64 / cache_total as f64;
        let prev_total = cache_prev + calls_prev;
        let trend = if prev_total >= 10 {
            let rate_prev = cache_prev as f64 / prev_total as f64;
            trend_arrow(rate_now - rate_prev, 0.05)
        } else {
            ""
        };
        segments.push(format!(
            "{DIM}ygg cache{RESET} {GREEN}{cache_hits}/{cache_total}{RESET}{trend}"
        ));
    }

    // ADR 0015 Phase 1 dropped retrieval-as-memory; the `recalls/24h`
    // and `recall N/M (X%)` segments measured a feature we no longer
    // run, so they're gone. Cache-hit segment stays — embedding still
    // fires from task dupe detection and manual searches.

    println!("{}", segments.join(" · "));
    Ok(())
}

/// Pull the user's `effortLevel` (e.g. "xhigh") from
/// `~/.claude/settings.json`. Best-effort — returns None if the file
/// is missing, malformed, or the field is absent.
fn read_effort_level() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::PathBuf::from(home).join(".claude/settings.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("effortLevel")
        .and_then(|x| x.as_str())
        .map(String::from)
}

/// Render a delta as a trend glyph. `delta` is the signed change (new - old
/// for absolute metrics, or (new - old)/old for rates). `flat_band` defines
/// the threshold below which we consider the movement noise.
fn trend_arrow(delta: f64, flat_band: f64) -> &'static str {
    if delta > flat_band {
        "\x1b[38;5;114m ↑\x1b[0m" // green up
    } else if delta < -flat_band {
        "\x1b[38;5;203m ↓\x1b[0m" // red down
    } else {
        "\x1b[2m ─\x1b[0m" // dim flat
    }
}

/// Pull a session token count from the Claude Code statusline JSON.
/// CC has shipped several shapes over releases; check each, fall back to a
/// transcript-file-size estimate (~4 chars per token).
fn token_count(j: &serde_json::Value) -> Option<i64> {
    let direct = [
        "/token_usage/total_tokens",
        "/tokens/total",
        "/usage/total_tokens",
    ];
    for path in direct {
        if let Some(n) = j.pointer(path).and_then(|v| v.as_i64()) {
            if n > 0 {
                return Some(n);
            }
        }
    }
    // Input + output sum, multiple spellings.
    for (in_p, out_p) in [
        ("/token_usage/input_tokens", "/token_usage/output_tokens"),
        ("/tokens/input", "/tokens/output"),
        ("/usage/input_tokens", "/usage/output_tokens"),
    ] {
        let i = j.pointer(in_p).and_then(|v| v.as_i64()).unwrap_or(0);
        let o = j.pointer(out_p).and_then(|v| v.as_i64()).unwrap_or(0);
        if i + o > 0 {
            return Some(i + o);
        }
    }
    // Fallback: parse the last `usage` entry from the transcript JSONL —
    // same signal CC's own status line uses (cache_read + cache_creation +
    // input + output). Reads only the tail of the file.
    let transcript = j.get("transcript_path").and_then(|v| v.as_str())?;
    if let Some(n) = last_usage_tokens_from_transcript(std::path::Path::new(transcript)) {
        return Some(n);
    }
    // Very last resort: bytes / 30 is much closer to reality for JSONL than
    // the old /8 heuristic, which was ~4x high.
    let bytes = std::fs::metadata(transcript).ok()?.len() as i64;
    Some((bytes / 30).max(0))
}

fn last_usage_tokens_from_transcript(path: &std::path::Path) -> Option<i64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let tail_start = len.saturating_sub(200_000);
    file.seek(SeekFrom::Start(tail_start)).ok()?;
    let mut buf = String::new();
    file.take(200_000).read_to_string(&mut buf).ok()?;
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
        let total = cr + cc + inp + out;
        if total > 0 {
            return Some(total);
        }
    }
    None
}

fn format_tokens(n: i64) -> String {
    // Always K up to 10M (900K ≠ 1M is a big deal); M only when the
    // thousands digit stops carrying useful information. No decimal — we
    // want to see the thousands place, not lose it to rounding.
    if n >= 10_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        format!("{n}")
    }
}
