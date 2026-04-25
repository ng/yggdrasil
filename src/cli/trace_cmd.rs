//! `ygg trace` — for each recent user turn, walk the events table and
//! render what Yggdrasil actually did: embed, retrieve, score, filter,
//! emit, reference (if digested). The point is transparency — does the
//! system understand what the human thinks it does? If the rendered
//! trace doesn't match your mental model, something's miscalibrated.

use chrono::{DateTime, Utc};
use uuid::Uuid;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[38;5;81m";
const GREEN: &str = "\x1b[38;5;114m";
const BLUE: &str = "\x1b[38;5;111m";
const RED: &str = "\x1b[38;5;203m";
const GRAY: &str = "\x1b[38;5;245m";

pub async fn execute(
    pool: &sqlx::PgPool,
    last: i64,
    agent_name: Option<&str>,
    full: bool,
) -> Result<(), anyhow::Error> {
    // Pull the most recent N user_message NodeWritten events for the agent.
    let prompts: Vec<(Uuid, Uuid, String, DateTime<Utc>, serde_json::Value)> =
        if let Some(name) = agent_name {
            sqlx::query_as(
                r#"SELECT id, agent_id, agent_name, created_at, payload
               FROM events
               WHERE event_kind::text = 'node_written'
                 AND agent_name = $1
                 AND payload->>'kind' = 'user_message'
               ORDER BY created_at DESC LIMIT $2"#,
            )
            .bind(name)
            .bind(last)
            .fetch_all(pool)
            .await?
        } else {
            sqlx::query_as(
                r#"SELECT id, agent_id, agent_name, created_at, payload
               FROM events
               WHERE event_kind::text = 'node_written'
                 AND payload->>'kind' = 'user_message'
               ORDER BY created_at DESC LIMIT $1"#,
            )
            .bind(last)
            .fetch_all(pool)
            .await?
        };

    if prompts.is_empty() {
        println!(
            "No user turns found{}.",
            agent_name
                .map(|n| format!(" for agent {n}"))
                .unwrap_or_default()
        );
        return Ok(());
    }

    // Render oldest-first for chronological reading.
    for (event_id, agent_id, agent_name, ts, payload) in prompts.into_iter().rev() {
        render_turn(pool, event_id, agent_id, &agent_name, ts, &payload, full).await?;
        println!();
    }
    Ok(())
}

async fn render_turn(
    pool: &sqlx::PgPool,
    _node_write_event: Uuid,
    agent_id: Uuid,
    agent_name: &str,
    ts: DateTime<Utc>,
    node_payload: &serde_json::Value,
    full: bool,
) -> Result<(), anyhow::Error> {
    let snippet = node_payload
        .get("snippet")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let tokens = node_payload
        .get("tokens")
        .and_then(|t| t.as_i64())
        .unwrap_or(0);
    let local_ts = ts.with_timezone(&chrono::Local).format("%H:%M:%S");

    // Header
    println!("{BOLD}Turn{RESET} {DIM}@ {local_ts}{RESET}  {GRAY}{agent_name}{RESET}");
    println!(
        "  {DIM}prompt:{RESET} \"{}\"  {DIM}(~{tokens}t){RESET}",
        truncate(snippet, 90)
    );

    // Events in a wide window around this turn. The first ~8s catches the
    // retrieval pipeline; hit_referenced fires at digest time much later,
    // so we stretch the upper bound to 2h to catch it.
    let lo = ts - chrono::Duration::seconds(1);
    let hi = ts + chrono::Duration::hours(2);
    let events: Vec<(DateTime<Utc>, String, serde_json::Value)> = sqlx::query_as(
        r#"SELECT created_at, event_kind::text, payload
           FROM events
           WHERE agent_id = $1 AND created_at >= $2 AND created_at <= $3
           ORDER BY created_at ASC, id ASC"#,
    )
    .bind(agent_id)
    .bind(lo)
    .bind(hi)
    .fetch_all(pool)
    .await?;

    let mut embed: Option<&serde_json::Value> = None;
    let mut cache_hit: Option<&serde_json::Value> = None;
    let mut scoring: Vec<&serde_json::Value> = Vec::new();
    let mut classifier: Vec<&serde_json::Value> = Vec::new();
    let mut hits: Vec<&serde_json::Value> = Vec::new();
    let mut referenced: Vec<&serde_json::Value> = Vec::new();
    let mut redactions: Vec<&serde_json::Value> = Vec::new();

    for (_, kind, payload) in &events {
        match kind.as_str() {
            "embedding_call" if embed.is_none() => embed = Some(payload),
            "embedding_cache_hit" if cache_hit.is_none() => cache_hit = Some(payload),
            "scoring_decision" => scoring.push(payload),
            "classifier_decision" => classifier.push(payload),
            "similarity_hit" => hits.push(payload),
            "hit_referenced" => referenced.push(payload),
            "redaction_applied" => redactions.push(payload),
            _ => {}
        }
    }

    // Embed/cache
    if let Some(ch) = cache_hit {
        let ms = ch.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let chars = ch.get("input_chars").and_then(|v| v.as_u64()).unwrap_or(0);
        println!(
            "  ├─ {GREEN}embed{RESET}     cache {BOLD}hit{RESET}  {DIM}{chars} chars  {ms}ms{RESET}"
        );
    } else if let Some(e) = embed {
        let ms = e.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let chars = e.get("input_chars").and_then(|v| v.as_u64()).unwrap_or(0);
        let ok = e.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        let status = if ok {
            format!("{GREEN}ok{RESET}")
        } else {
            format!("{RED}fail{RESET}")
        };
        println!("  ├─ {CYAN}embed{RESET}     ollama  {DIM}{chars} chars  {ms}ms{RESET}  {status}");
    } else {
        println!("  ├─ {DIM}embed     (skipped — ollama down?){RESET}");
    }

    if !redactions.is_empty() {
        for r in &redactions {
            let total = r.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
            println!("  ├─ {RED}redact{RESET}    {total} secret(s) removed from node content");
        }
    }

    // Retrieval
    let n_retrieved = scoring.len().max(hits.len());
    let retrieve_line = if n_retrieved > 0 {
        format!(
            "  ├─ {CYAN}retrieve{RESET}  hybrid (pgvector + tsvector)  {DIM}→ {n_retrieved} candidate(s){RESET}"
        )
    } else {
        format!("  ├─ {DIM}retrieve  0 candidates (no matches){RESET}")
    };
    println!("{retrieve_line}");

    // Scoring table
    if !scoring.is_empty() {
        println!(
            "  │   {DIM}mechanical scoring (drops only — kept candidates shown below):{RESET}"
        );
        for s in &scoring {
            let snip = s.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            let src = s
                .get("source_agent")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let total = s
                .get("components")
                .and_then(|c| c.get("total"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let reason = s.get("drop_reason").and_then(|v| v.as_str()).unwrap_or("");
            println!(
                "  │    {RED}✗{RESET} {total:.2}  {DIM}from {src}{RESET}  {DIM}({reason}){RESET}  \"{}\"",
                truncate(snip, 40)
            );
        }
    }

    // Classifier if enabled
    if !classifier.is_empty() {
        let kept = classifier
            .iter()
            .filter(|c| c.get("kept").and_then(|v| v.as_bool()).unwrap_or(false))
            .count();
        let bypassed = classifier
            .iter()
            .filter(|c| c.get("bypassed").and_then(|v| v.as_bool()).unwrap_or(false))
            .count();
        println!(
            "  ├─ {CYAN}classifier{RESET}  kept={kept} bypassed={bypassed}  {DIM}(llm rerank){RESET}"
        );
    }

    // Emitted hits (last node in the pipeline before the agent sees them)
    let suppressed = n_retrieved.saturating_sub(
        hits.len()
            + scoring
                .iter()
                .filter(|s| !s.get("kept").and_then(|v| v.as_bool()).unwrap_or(true))
                .count(),
    );
    let _ = suppressed;

    // Build a set of referenced source_node_ids so each emit line can be
    // marked individually ("did the next turn actually use THIS hit?").
    let mut referenced_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &referenced {
        if let Some(sid) = r.get("source_node_id").and_then(|v| v.as_str()) {
            referenced_ids.insert(sid.to_string());
        }
    }

    let mut ref_count = 0usize;
    if hits.is_empty() {
        println!("  ├─ {DIM}emit      0 lines to agent{RESET}");
    } else {
        println!(
            "  ├─ {GREEN}emit{RESET}      {} line(s) to agent{RESET}",
            hits.len()
        );
        for h in &hits {
            let score = h
                .get("total_score")
                .or_else(|| h.get("similarity"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let src = h
                .get("source_agent")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let snip = h.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            let source_id = h
                .get("source_node_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let label = strength_label(score);
            let label_colored = match label {
                "strong" => format!("{GREEN}{label}{RESET}"),
                "recall" => format!("{BLUE}{label}{RESET}"),
                _ => format!("{DIM}{label}{RESET}"),
            };
            let was_referenced = !source_id.is_empty() && referenced_ids.contains(source_id);
            if was_referenced {
                ref_count += 1;
            }
            let verdict = if was_referenced {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{DIM}·{RESET}")
            };
            let shown = if full {
                snip.to_string()
            } else {
                truncate(snip, 50)
            };
            println!(
                "  │    {verdict} {label_colored} {score:.2}  {DIM}from {src}{RESET}  \"{shown}\""
            );
        }
    }

    // Headline verdict — the whole reason the user asked for this view.
    if hits.is_empty() {
        println!("  └─ {DIM}referenced  nothing surfaced to reference{RESET}");
    } else {
        let rate = (ref_count as f64 / hits.len() as f64 * 100.0) as i64;
        let rate_colored = if ref_count == 0 && referenced_ids.is_empty() {
            format!(
                "{DIM}{ref_count}/{} ({rate}%)  (digest hasn't scored this turn yet){RESET}",
                hits.len()
            )
        } else if rate >= 50 {
            format!("{GREEN}{BOLD}{ref_count}/{} ({rate}%){RESET}", hits.len())
        } else if rate >= 20 {
            format!(
                "\x1b[38;5;221m{BOLD}{ref_count}/{} ({rate}%){RESET}",
                hits.len()
            )
        } else {
            format!("{DIM}{ref_count}/{} ({rate}%){RESET}", hits.len())
        };
        println!("  └─ {CYAN}referenced{RESET}  {rate_colored}");
    }

    Ok(())
}

fn strength_label(score: f64) -> &'static str {
    if score >= 0.6 {
        "strong"
    } else if score >= 0.3 {
        "recall"
    } else {
        "faint"
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}
