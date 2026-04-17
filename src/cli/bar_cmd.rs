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
    let in_tok = j.pointer("/token_usage/input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let out_tok = j.pointer("/token_usage/output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let tok_total = in_tok + out_tok;

    // Look up today's cache savings and inference counts for a "did Yggdrasil
    // help me today?" at-a-glance signal.
    let since = Utc::now() - Duration::hours(24);
    let (cache_hits, embed_calls): (i64, i64) = sqlx::query_as(
        r#"SELECT
             COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit'),
             COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call')
           FROM events WHERE created_at >= $1"#,
    )
    .bind(since)
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0));

    let hits_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'similarity_hit' AND created_at >= $1",
    )
    .bind(since)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let mut segments: Vec<String> = Vec::new();

    // Context % + bar glyph based on pressure tier
    let bar_color = if pct >= 90 { YELL } else if pct >= 75 { CYAN } else { GREEN };
    segments.push(format!("{bar_color}▊{RESET} {BOLD}{pct:>3}%{RESET}"));

    // Token usage
    if tok_total > 0 {
        segments.push(format!("{}", format_tokens(tok_total)));
    }

    // Session cost — always 2 dp
    if cost_usd > 0.0 {
        segments.push(format!("${:.2}", cost_usd));
    }

    // Cache hit rate (24h rolling) — shows the system is working
    let cache_total = cache_hits + embed_calls;
    if cache_total > 0 {
        let rate = cache_hits as f64 / cache_total as f64 * 100.0;
        segments.push(format!(
            "{GREEN}cache {rate:>2.0}%{RESET} {DIM}({cache_hits} saved){RESET}"
        ));
    }

    // Similarity hits today — "Yggdrasil actually surfaced N memories for you"
    if hits_today > 0 {
        segments.push(format!("{DIM}{hits_today} recalls/24h{RESET}"));
    }

    println!("{}", segments.join(" · "));
    Ok(())
}

fn format_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M tok", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K tok", n as f64 / 1_000.0)
    } else {
        format!("{n} tok")
    }
}
