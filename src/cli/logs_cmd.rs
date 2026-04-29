use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::models::event::{Event, EventKind, EventRepo};

// ANSI colors
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[38;5;114m";
const YELLOW: &str = "\x1b[38;5;221m";
const CYAN: &str = "\x1b[38;5;81m";
const ORANGE: &str = "\x1b[38;5;208m";
const RED: &str = "\x1b[38;5;203m";
const BLUE: &str = "\x1b[38;5;111m";
const GRAY: &str = "\x1b[38;5;245m";

pub async fn execute(
    pool: &PgPool,
    follow: bool,
    tail: i64,
    agent_name: Option<&str>,
    kinds: Option<Vec<String>>,
    cc_session_id: Option<&str>,
) -> Result<(), anyhow::Error> {
    let _ = EventRepo::new(pool); // reserved for back-compat paths
    let divider = format!("{DIM}{}{RESET}", "─".repeat(72));

    let header_suffix = {
        let mut parts = Vec::new();
        if let Some(n) = agent_name {
            parts.push(format!("agent={n}"));
        }
        if let Some(ks) = &kinds {
            parts.push(format!("kind={}", ks.join(",")));
        }
        if let Some(s) = cc_session_id {
            parts.push(format!("session={s}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("  {DIM}{}{RESET}", parts.join(" · "))
        }
    };
    println!("{divider}");
    println!("  {BOLD}ygg event stream{RESET}{header_suffix}");
    println!("{divider}");

    let mut recent =
        filtered_recent(pool, tail, agent_name, kinds.as_deref(), cc_session_id).await?;
    recent.reverse();
    for event in &recent {
        print_event(event);
    }

    if !follow {
        println!("{DIM}  (use --follow to stream live){RESET}");
        return Ok(());
    }

    println!("{DIM}  streaming… (ctrl-c to exit){RESET}");

    let mut cursor: DateTime<Utc> = recent.last().map(|e| e.created_at).unwrap_or_else(Utc::now);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let new_events = filtered_since(
            pool,
            cursor,
            agent_name,
            kinds.as_deref(),
            cc_session_id,
            50,
        )
        .await?;
        for event in &new_events {
            print_event(event);
            cursor = event.created_at;
        }
    }
}

async fn filtered_recent(
    pool: &PgPool,
    tail: i64,
    agent_name: Option<&str>,
    kinds: Option<&[String]>,
    cc_session_id: Option<&str>,
) -> Result<Vec<Event>, anyhow::Error> {
    let kinds_owned: Vec<String> = kinds.map(|k| k.to_vec()).unwrap_or_default();
    let rows: Vec<Event> = sqlx::query_as::<_, Event>(
        r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
             FROM events
            WHERE ($1::text IS NULL OR agent_name = $1)
              AND ($2::text[] IS NULL OR array_length($2, 1) IS NULL OR event_kind::text = ANY($2))
              AND ($3::text IS NULL OR cc_session_id = $3)
            ORDER BY created_at DESC
            LIMIT $4"#,
    )
    .bind(agent_name)
    .bind(if kinds_owned.is_empty() {
        None
    } else {
        Some(kinds_owned.clone())
    })
    .bind(cc_session_id)
    .bind(tail)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn filtered_since(
    pool: &PgPool,
    since: DateTime<Utc>,
    agent_name: Option<&str>,
    kinds: Option<&[String]>,
    cc_session_id: Option<&str>,
    limit: i64,
) -> Result<Vec<Event>, anyhow::Error> {
    let kinds_owned: Vec<String> = kinds.map(|k| k.to_vec()).unwrap_or_default();
    let rows: Vec<Event> = sqlx::query_as::<_, Event>(
        r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
             FROM events
            WHERE created_at > $1
              AND ($2::text IS NULL OR agent_name = $2)
              AND ($3::text[] IS NULL OR array_length($3, 1) IS NULL OR event_kind::text = ANY($3))
              AND ($4::text IS NULL OR cc_session_id = $4)
            ORDER BY created_at ASC
            LIMIT $5"#,
    )
    .bind(since)
    .bind(agent_name)
    .bind(if kinds_owned.is_empty() {
        None
    } else {
        Some(kinds_owned.clone())
    })
    .bind(cc_session_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Render one event as a single line. We deliberately avoid column
/// alignment: variable-width data (agent names, snippets, numeric counts)
/// makes columnar layouts sheer in ways that are annoying to fix and
/// never look good across every event kind. Instead we emit a scannable
/// delimited form — `time · symbol label · agent · detail` — each field
/// stands on its own, colors segment the fields visually.
fn print_event(event: &Event) {
    let ts = event
        .created_at
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S");
    let (color, symbol) = kind_style(&event.event_kind);
    let label = event.event_kind.label();
    let agent = truncate(&event.agent_name, 24);
    let detail = format_payload(&event.event_kind, &event.payload);

    println!(
        "{DIM}{ts}{RESET} {color}{symbol}{label}{RESET} {SEP} {GRAY}{agent}{RESET} {SEP} {detail}"
    );
}

const SEP: &str = "\x1b[38;5;240m·\x1b[0m";

fn kind_style(kind: &EventKind) -> (&'static str, &'static str) {
    match kind {
        EventKind::NodeWritten => (GREEN, "●"),
        EventKind::LockAcquired => (YELLOW, "⚿"),
        EventKind::LockReleased => (DIM, "○"),
        EventKind::DigestWritten => (CYAN, "◈"),
        EventKind::SimilarityHit => (BLUE, "≈"),
        EventKind::CorrectionDetected => (RED, "✗"),
        EventKind::HookFired => (ORANGE, "▸"),
        EventKind::EmbeddingCall => (CYAN, "⚡"),
        EventKind::TaskCreated => (GREEN, "✚"),
        EventKind::TaskStatusChanged => (YELLOW, "◆"),
        EventKind::Remembered => (BLUE, "♦"),
        EventKind::EmbeddingCacheHit => (GREEN, "⚡"),
        EventKind::ClassifierDecision => (CYAN, "⚖"),
        EventKind::ScoringDecision => (GRAY, "·"),
        EventKind::RedactionApplied => (RED, "✂"),
        EventKind::HitReferenced => (GREEN, "✓"),
        EventKind::AgentStateChanged => (BLUE, "↪"),
        EventKind::Message => (CYAN, "✉"),
        EventKind::RunScheduled => (DIM, "□"),
        EventKind::RunClaimed => (BLUE, "▶"),
        EventKind::RunTerminal => (GREEN, "■"),
        EventKind::RunRetry => (YELLOW, "↻"),
        EventKind::SchedulerTick => (DIM, "·"),
        EventKind::SchedulerError => (RED, "!"),
        EventKind::AgentStaleWarning => (YELLOW, "⌛"),
    }
}

fn format_payload(kind: &EventKind, p: &serde_json::Value) -> String {
    match kind {
        EventKind::NodeWritten => {
            let kind = p["kind"].as_str().unwrap_or("node");
            let tok = p["tokens"].as_i64().unwrap_or(0);
            let snip = p["snippet"].as_str().unwrap_or("");
            // Humanize kind; omit raw token count (it's an estimate, not load-bearing
            // to the human reader). The snippet is what actually matters.
            let kind_label = match kind {
                "user_message" => "user",
                "assistant_message" => "assistant",
                "tool_call" => "tool call",
                "tool_result" => "tool result",
                "digest" => "digest",
                "directive" => "directive",
                "human_override" => "override",
                "system" => "system",
                _ => kind,
            };
            format!(
                "{CYAN}{kind_label:<11}{RESET} {DIM}~{tok:>3}t{RESET}  {}",
                truncate(snip, 50)
            )
        }
        EventKind::LockAcquired | EventKind::LockReleased => p["resource"]
            .as_str()
            .map(|r| truncate(r, 60).to_string())
            .unwrap_or_default(),
        EventKind::DigestWritten => {
            let turns = p["turns"].as_i64().unwrap_or(0);
            let corr = p["corrections"].as_i64().unwrap_or(0);
            let reinf = p["reinforcements"].as_i64().unwrap_or(0);
            format!(
                "{DIM}{turns:>4} turns{RESET}  {RED}{corr:>2} corrections{RESET}  {GREEN}{reinf:>2} reinforcements{RESET}"
            )
        }
        EventKind::SimilarityHit => {
            let score = p["total_score"]
                .as_f64()
                .unwrap_or_else(|| p["similarity"].as_f64().unwrap_or(0.0));
            let src = p["source_agent"].as_str().unwrap_or("?");
            let snip = p["snippet"].as_str().unwrap_or("");
            let label = if score >= 0.6 {
                format!("{GREEN}strong{RESET}")
            } else if score >= 0.3 {
                format!("{BLUE}recall{RESET}")
            } else {
                format!("{DIM}faint{RESET}")
            };
            format!(
                "{label} {DIM}{score:.2} from {src}{RESET}  {}",
                truncate(snip, 40)
            )
        }
        EventKind::CorrectionDetected => {
            let fb = p["feedback"].as_str().unwrap_or("");
            let sent = p["sentiment"].as_str().unwrap_or("");
            format!("{RED}{sent}{RESET}  {}", truncate(fb, 55))
        }
        EventKind::HookFired => p["hook"].as_str().unwrap_or("").to_string(),
        EventKind::EmbeddingCall => {
            let model = p["model"].as_str().unwrap_or("?");
            let ms = p["latency_ms"].as_u64().unwrap_or(0);
            let chars = p["input_chars"].as_u64().unwrap_or(0);
            let ok = p["success"].as_bool().unwrap_or(false);
            let status = if ok {
                format!("{GREEN}ok{RESET}")
            } else {
                format!("{RED}fail{RESET}")
            };
            format!("{CYAN}{model:<11}{RESET} {chars:>4} chars  {ms:>4}ms  {status}")
        }
        EventKind::TaskCreated => {
            let rref = p["ref"].as_str().unwrap_or("?");
            let title = p["title"].as_str().unwrap_or("");
            let kind = p["kind"].as_str().unwrap_or("task");
            let pri = p["priority"].as_i64().unwrap_or(2);
            format!(
                "{GREEN}{rref}{RESET}  {DIM}P{pri} {kind}{RESET}  {}",
                truncate(title, 50)
            )
        }
        EventKind::TaskStatusChanged => {
            let rref = p["ref"].as_str().unwrap_or("?");
            let to = p["to"].as_str().unwrap_or("?");
            let reason = p["reason"].as_str();
            let extra = reason
                .map(|r| format!("  {DIM}{}{RESET}", truncate(r, 40)))
                .unwrap_or_default();
            format!("{YELLOW}{rref}{RESET} → {to}{extra}")
        }
        EventKind::Remembered => {
            let tok = p["tokens"].as_i64().unwrap_or(0);
            let snip = p["snippet"].as_str().unwrap_or("");
            format!(
                "{CYAN}directive{RESET}   {DIM}~{tok}t{RESET}  {}",
                truncate(snip, 50)
            )
        }
        EventKind::EmbeddingCacheHit => {
            let model = p["model"].as_str().unwrap_or("?");
            let ms = p["latency_ms"].as_u64().unwrap_or(0);
            let chars = p["input_chars"].as_u64().unwrap_or(0);
            let purpose = p["purpose"].as_str().unwrap_or("");
            format!(
                "{GREEN}{model:<11}{RESET} {chars:>4} chars  {ms:>4}ms  {DIM}{purpose} (cached){RESET}"
            )
        }
        EventKind::ClassifierDecision => {
            let score = p["score"].as_f64().unwrap_or(0.0);
            let kept = p["kept"].as_bool().unwrap_or(false);
            let bypassed = p["bypassed"].as_bool().unwrap_or(false);
            let src = p["source_agent"].as_str().unwrap_or("?");
            let snip = p["snippet"].as_str().unwrap_or("");
            let verdict = if bypassed {
                format!("{DIM}bypass{RESET}")
            } else if kept {
                format!("{GREEN}keep{RESET}")
            } else {
                format!("{RED}drop{RESET}")
            };
            format!(
                "{verdict} {CYAN}score={score:.2}{RESET} {DIM}from {src}{RESET}  {}",
                truncate(snip, 40)
            )
        }
        EventKind::ScoringDecision => {
            let kept = p["kept"].as_bool().unwrap_or(false);
            let reason = p["drop_reason"].as_str().unwrap_or("");
            let total = p["components"]["total"].as_f64().unwrap_or(0.0);
            let snip = p["snippet"].as_str().unwrap_or("");
            let src = p["source_agent"].as_str().unwrap_or("?");
            let verdict = if kept {
                format!("{GREEN}keep{RESET}")
            } else {
                format!("{RED}drop{RESET}")
            };
            let extra = if !reason.is_empty() && !kept {
                format!(" {DIM}({reason}){RESET}")
            } else {
                String::new()
            };
            format!(
                "{verdict}{extra} {DIM}{total:.2} from {src}{RESET}  {}",
                truncate(snip, 35)
            )
        }
        EventKind::RedactionApplied => {
            let total = p["total"].as_i64().unwrap_or(0);
            let node_kind = p["node_kind"].as_str().unwrap_or("");
            let kinds = p["kinds"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            format!("{RED}{total} redacted{RESET} {DIM}in {node_kind} · {kinds}{RESET}")
        }
        EventKind::HitReferenced => {
            let overlap = p["overlap"].as_f64().unwrap_or(0.0);
            let method = p["method"].as_str().unwrap_or("");
            format!("{GREEN}referenced{RESET} {DIM}overlap={overlap:.2} method={method}{RESET}")
        }
        EventKind::AgentStateChanged => {
            let from = p["from"].as_str().unwrap_or("?");
            let to = p["to"].as_str().unwrap_or("?");
            let tool = p["tool"].as_str();
            let suffix = tool
                .map(|t| format!(" {DIM}({t}){RESET}"))
                .unwrap_or_default();
            format!("{BLUE}{from}{RESET} → {to}{suffix}")
        }
        EventKind::Message => {
            let body = p["body"].as_str().unwrap_or("");
            format!("{CYAN}msg{RESET}  {}", truncate(body, 60))
        }
        EventKind::RunScheduled => {
            let task = p["task_ref"].as_str().unwrap_or("?");
            let attempt = p["attempt"].as_i64().unwrap_or(1);
            format!("{DIM}{task} attempt={attempt}{RESET}")
        }
        EventKind::RunClaimed => {
            let task = p["task_ref"].as_str().unwrap_or("?");
            let agent = p["agent"].as_str().unwrap_or("?");
            format!("{BLUE}{task}{RESET} → {agent}")
        }
        EventKind::RunTerminal => {
            let task = p["task_ref"].as_str().unwrap_or("?");
            let state = p["state"].as_str().unwrap_or("?");
            let reason = p["reason"].as_str();
            let color = match state {
                "succeeded" => GREEN,
                "failed" | "crashed" | "poison" => RED,
                "cancelled" => DIM,
                _ => GRAY,
            };
            let reason_extra = reason
                .filter(|r| *r != "ok")
                .map(|r| format!("  {DIM}{}{RESET}", r))
                .unwrap_or_default();
            format!("{color}{task}{RESET} {state}{reason_extra}")
        }
        EventKind::RunRetry => {
            let task = p["task_ref"].as_str().unwrap_or("?");
            let attempt = p["attempt"].as_i64().unwrap_or(0);
            let backoff_ms = p["backoff_ms"].as_i64().unwrap_or(0);
            format!("{YELLOW}{task}{RESET} attempt {attempt} after {DIM}{backoff_ms}ms{RESET}")
        }
        EventKind::SchedulerTick => {
            let spawned = p["dispatched"].as_i64().unwrap_or(0);
            let done = p["finalized"].as_i64().unwrap_or(0);
            let retried = p["retried"].as_i64().unwrap_or(0);
            format!("{DIM}spawned={spawned} done={done} retried={retried}{RESET}")
        }
        EventKind::SchedulerError => {
            let msg = p["error"].as_str().unwrap_or("");
            format!("{RED}{}{RESET}", truncate(msg, 70))
        }
        EventKind::AgentStaleWarning => {
            let state = p["current_state"].as_str().unwrap_or("?");
            let last = p["last_update"].as_str().unwrap_or("");
            format!(
                "{YELLOW}stale {state}{RESET} {DIM}last={}{RESET}",
                truncate(last, 25)
            )
        }
    }
}

/// Truncate at a char boundary, not a byte boundary. Prevents panics on
/// multi-byte glyphs (e.g. snippets with ─ box-drawing chars from our own
/// log output that made its way into the DAG as a node).
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Walk backwards from `max` to the nearest char boundary.
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
