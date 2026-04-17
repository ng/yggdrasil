use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::models::event::{Event, EventKind, EventRepo};

// ANSI colors
const RESET:  &str = "\x1b[0m";
const DIM:    &str = "\x1b[2m";
const BOLD:   &str = "\x1b[1m";
const GREEN:  &str = "\x1b[38;5;114m";
const YELLOW: &str = "\x1b[38;5;221m";
const CYAN:   &str = "\x1b[38;5;81m";
const ORANGE: &str = "\x1b[38;5;208m";
const RED:    &str = "\x1b[38;5;203m";
const BLUE:   &str = "\x1b[38;5;111m";
const GRAY:   &str = "\x1b[38;5;245m";

pub async fn execute(
    pool: &PgPool,
    follow: bool,
    tail: i64,
    agent_name: Option<&str>,
) -> Result<(), anyhow::Error> {
    let repo = EventRepo::new(pool);

    let divider = format!("{DIM}{}{RESET}", "─".repeat(72));

    // Print header
    println!("{divider}");
    println!(
        "  {BOLD}ygg event stream{RESET}{}",
        agent_name.map(|n| format!("  {DIM}agent={n}{RESET}")).unwrap_or_default()
    );
    println!("{divider}");

    // Show recent history first
    let mut recent = repo.list_recent(tail, agent_name).await?;
    recent.reverse(); // oldest first
    for event in &recent {
        print_event(event);
    }

    if !follow {
        println!("{DIM}  (use --follow to stream live){RESET}");
        return Ok(());
    }

    println!("{DIM}  streaming… (ctrl-c to exit){RESET}");

    let mut cursor: DateTime<Utc> = recent
        .last()
        .map(|e| e.created_at)
        .unwrap_or_else(Utc::now);

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let new_events = repo.list_since(cursor, agent_name, 50).await?;
        for event in &new_events {
            print_event(event);
            cursor = event.created_at;
        }
    }
}

fn print_event(event: &Event) {
    let ts = event.created_at.with_timezone(&chrono::Local).format("%H:%M:%S");
    let (color, symbol) = kind_style(&event.event_kind);
    let label = format!("{:<18}", event.event_kind.label());
    let agent = format!("{:<16}", truncate(&event.agent_name, 16));
    let detail = format_payload(&event.event_kind, &event.payload);

    println!(
        "{DIM}{ts}{RESET}  {color}{symbol} {label}{RESET}  {GRAY}{agent}{RESET}  {detail}"
    );
}

fn kind_style(kind: &EventKind) -> (&'static str, &'static str) {
    match kind {
        EventKind::NodeWritten       => (GREEN,  "●"),
        EventKind::LockAcquired      => (YELLOW, "⚿"),
        EventKind::LockReleased      => (DIM,    "○"),
        EventKind::DigestWritten     => (CYAN,   "◈"),
        EventKind::SimilarityHit     => (BLUE,   "≈"),
        EventKind::CorrectionDetected => (RED,   "✗"),
        EventKind::HookFired         => (ORANGE, "▸"),
        EventKind::EmbeddingCall     => (CYAN,   "⚡"),
        EventKind::TaskCreated       => (GREEN,  "✚"),
        EventKind::TaskStatusChanged => (YELLOW, "◆"),
        EventKind::Remembered        => (BLUE,   "♦"),
        EventKind::EmbeddingCacheHit => (GREEN,  "⚡"),
        EventKind::ClassifierDecision => (CYAN,  "⚖"),
    }
}

fn format_payload(kind: &EventKind, p: &serde_json::Value) -> String {
    match kind {
        EventKind::NodeWritten => {
            let tok = p["tokens"].as_i64().unwrap_or(0);
            let snip = p["snippet"].as_str().unwrap_or("");
            format!("{DIM}{tok}tok{RESET}  {}", truncate(snip, 50))
        }
        EventKind::LockAcquired | EventKind::LockReleased => {
            p["resource"].as_str()
                .map(|r| truncate(r, 60).to_string())
                .unwrap_or_default()
        }
        EventKind::DigestWritten => {
            let turns = p["turns"].as_i64().unwrap_or(0);
            let corr  = p["corrections"].as_i64().unwrap_or(0);
            let reinf = p["reinforcements"].as_i64().unwrap_or(0);
            format!(
                "{DIM}{turns} turns{RESET}  {RED}{corr} corrections{RESET}  {GREEN}{reinf} reinforcements{RESET}"
            )
        }
        EventKind::SimilarityHit => {
            let sim  = p["similarity"].as_f64().unwrap_or(0.0) * 100.0;
            let src  = p["source_agent"].as_str().unwrap_or("?");
            let snip = p["snippet"].as_str().unwrap_or("");
            format!("{BLUE}sim={sim:.0}%{RESET} {DIM}from {src}{RESET}  {}", truncate(snip, 40))
        }
        EventKind::CorrectionDetected => {
            let fb   = p["feedback"].as_str().unwrap_or("");
            let sent = p["sentiment"].as_str().unwrap_or("");
            format!("{RED}{sent}{RESET}  {}", truncate(fb, 55))
        }
        EventKind::HookFired => {
            p["hook"].as_str().unwrap_or("").to_string()
        }
        EventKind::EmbeddingCall => {
            let model = p["model"].as_str().unwrap_or("?");
            let ms    = p["latency_ms"].as_u64().unwrap_or(0);
            let chars = p["input_chars"].as_u64().unwrap_or(0);
            let ok    = p["success"].as_bool().unwrap_or(false);
            let status = if ok { format!("{GREEN}ok{RESET}") } else { format!("{RED}fail{RESET}") };
            format!("{CYAN}{model}{RESET}  {chars} chars  {ms}ms  {status}")
        }
        EventKind::TaskCreated => {
            let rref = p["ref"].as_str().unwrap_or("?");
            let title = p["title"].as_str().unwrap_or("");
            let kind = p["kind"].as_str().unwrap_or("task");
            let pri = p["priority"].as_i64().unwrap_or(2);
            format!("{GREEN}{rref}{RESET}  {DIM}P{pri} {kind}{RESET}  {}", truncate(title, 50))
        }
        EventKind::TaskStatusChanged => {
            let rref = p["ref"].as_str().unwrap_or("?");
            let to = p["to"].as_str().unwrap_or("?");
            let reason = p["reason"].as_str();
            let extra = reason.map(|r| format!("  {DIM}{}{RESET}", truncate(r, 40))).unwrap_or_default();
            format!("{YELLOW}{rref}{RESET} → {to}{extra}")
        }
        EventKind::Remembered => {
            let tok = p["tokens"].as_i64().unwrap_or(0);
            let snip = p["snippet"].as_str().unwrap_or("");
            format!("{DIM}{tok}tok{RESET}  {}", truncate(snip, 55))
        }
        EventKind::EmbeddingCacheHit => {
            let model = p["model"].as_str().unwrap_or("?");
            let ms    = p["latency_ms"].as_u64().unwrap_or(0);
            let chars = p["input_chars"].as_u64().unwrap_or(0);
            let purpose = p["purpose"].as_str().unwrap_or("");
            format!("{GREEN}{model}{RESET}  {chars} chars  {ms}ms  {DIM}{purpose} (cached){RESET}")
        }
        EventKind::ClassifierDecision => {
            let score = p["score"].as_f64().unwrap_or(0.0);
            let kept = p["kept"].as_bool().unwrap_or(false);
            let bypassed = p["bypassed"].as_bool().unwrap_or(false);
            let src = p["source_agent"].as_str().unwrap_or("?");
            let snip = p["snippet"].as_str().unwrap_or("");
            let verdict = if bypassed { format!("{DIM}bypass{RESET}") }
                          else if kept { format!("{GREEN}keep{RESET}") }
                          else { format!("{RED}drop{RESET}") };
            format!("{verdict} {CYAN}score={score:.2}{RESET} {DIM}from {src}{RESET}  {}", truncate(snip, 40))
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}
