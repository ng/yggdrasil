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
const DIM:   &str = "\x1b[2m";
const CYAN:  &str = "\x1b[36m";
const GREEN: &str = "\x1b[38;5;114m";
const YELL:  &str = "\x1b[38;5;221m";
const BOLD:  &str = "\x1b[1m";

pub async fn execute(pool: &sqlx::PgPool) -> Result<(), anyhow::Error> {
    // Read Claude Code's JSON payload from stdin. Non-fatal if absent.
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    let j: serde_json::Value = serde_json::from_str(&input)
        .unwrap_or_else(|_| serde_json::Value::Null);

    let pct = j.pointer("/context_window/used_percentage")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as i64;
    let cost_usd = j.pointer("/cost/total_cost_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    // Claude Code's statusLine JSON doesn't reliably expose token_usage
    // under one path. Try several known spellings, then fall back to a
    // transcript-byte-size estimate (~4 chars per token). This way tokens
    // show up next to cost regardless of how CC labels the field.
    let tok_total: i64 = token_count(&j).unwrap_or(0);

    // Look up today's cache savings and inference counts, plus the
    // preceding 24h window for trend deltas. One roundtrip.
    let now_minus_24 = Utc::now() - Duration::hours(24);
    let now_minus_48 = Utc::now() - Duration::hours(48);
    let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"SELECT
             COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit' AND created_at >= $1),
             COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'      AND created_at >= $1),
             COUNT(*) FILTER (WHERE event_kind::text = 'similarity_hit'      AND created_at >= $1),
             COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit' AND created_at < $1 AND created_at >= $2),
             COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'      AND created_at < $1 AND created_at >= $2),
             COUNT(*) FILTER (WHERE event_kind::text = 'similarity_hit'      AND created_at < $1 AND created_at >= $2)
           FROM events"#,
    )
    .bind(now_minus_24)
    .bind(now_minus_48)
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0, 0, 0, 0, 0));
    let (cache_hits, embed_calls, hits_today, cache_prev, calls_prev, hits_prev) = row;

    let mut segments: Vec<String> = Vec::new();

    // Context usage — always labelled so it's not confusable with cache %.
    let bar_color = if pct >= 90 { YELL } else if pct >= 75 { CYAN } else { GREEN };
    segments.push(format!(
        "{bar_color}▊{RESET} {DIM}ctx{RESET} {BOLD}{pct}%{RESET}"
    ));

    // Tokens — styled like ctx (dim label + bold value), sits next to it.
    if tok_total > 0 {
        segments.push(format!("{DIM}tok{RESET} {BOLD}{}{RESET}", format_tokens(tok_total)));
    }

    // Session cost — 2dp.
    if cost_usd > 0.0 {
        segments.push(format!("{DIM}cost{RESET} {BOLD}${:.2}{RESET}", cost_usd));
    }

    // Cache — absolute "X of Y cached" with a trend arrow comparing the
    // current 24h hit rate to the preceding 24h's rate. Rising = pool
    // warming or workload getting more repetitive. Flat-band is ±5 pct pts.
    let cache_total = cache_hits + embed_calls;
    if cache_total > 0 {
        let rate_now = cache_hits as f64 / cache_total as f64;
        let prev_total = cache_prev + calls_prev;
        let trend = if prev_total >= 10 {
            let rate_prev = cache_prev as f64 / prev_total as f64;
            trend_arrow(rate_now - rate_prev, 0.05)
        } else {
            "" // Not enough history to make a trend claim.
        };
        segments.push(format!(
            "{GREEN}cache {cache_hits}/{cache_total}{RESET}{trend}"
        ));
    }

    // Recalls (last 24h) with a trend arrow comparing to the preceding 24h.
    // Flat-band is ±15% of the prior window count.
    if hits_today > 0 {
        let trend = if hits_prev >= 10 {
            let pct_change = (hits_today - hits_prev) as f64 / hits_prev as f64;
            trend_arrow(pct_change, 0.15)
        } else {
            ""
        };
        segments.push(format!("{DIM}{hits_today} recalls/24h{RESET}{trend}"));
    }

    println!("{}", segments.join(" · "));
    Ok(())
}

/// Render a delta as a trend glyph. `delta` is the signed change (new - old
/// for absolute metrics, or (new - old)/old for rates). `flat_band` defines
/// the threshold below which we consider the movement noise.
fn trend_arrow(delta: f64, flat_band: f64) -> &'static str {
    if delta > flat_band {
        "\x1b[38;5;114m ↑\x1b[0m"  // green up
    } else if delta < -flat_band {
        "\x1b[38;5;203m ↓\x1b[0m"  // red down
    } else {
        "\x1b[2m ─\x1b[0m"  // dim flat
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
            if n > 0 { return Some(n); }
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
        if i + o > 0 { return Some(i + o); }
    }
    // Fallback: estimate from the transcript on disk. CC reliably provides
    // `transcript_path`. ~4 chars per token is the standard rough estimate;
    // for a JSONL transcript with tool calls and structured content, this
    // tends to slightly under-count real tokens but is the right order.
    let transcript = j.get("transcript_path").and_then(|v| v.as_str())?;
    let bytes = std::fs::metadata(transcript).ok()?.len() as i64;
    // JSONL has heavy framing; halve the naive bytes/4 to account for it.
    Some((bytes / 8).max(0))
}

fn format_tokens(n: i64) -> String {
    // Return just the magnitude; the caller wraps with the dim "tok" label.
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}
