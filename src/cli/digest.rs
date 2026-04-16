use std::io::BufRead;

use crate::config::AppConfig;
use crate::embed::Embedder;
use crate::models::agent::AgentRepo;
use crate::models::event::{EventKind, EventRepo};
use crate::models::node::{NodeKind, NodeRepo};

use tracing::{debug, info, warn};

/// Called by the Stop hook. Parses the Claude Code transcript, extracts
/// corrections and a summary, writes a Digest node with embedding.
pub async fn execute(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    agent_name: &str,
    transcript_path: &str,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let node_repo = NodeRepo::new(pool);
    let event_repo = EventRepo::new(pool);

    let agent = match agent_repo.get_by_name(agent_name).await? {
        Some(a) => a,
        None => {
            debug!("digest: agent '{}' not found — skipping", agent_name);
            return Ok(());
        }
    };

    debug!("digest: reading transcript {}", transcript_path);
    let entries = parse_transcript(transcript_path);
    debug!("digest: {} turns parsed", entries.len());

    if entries.is_empty() {
        debug!("digest: empty transcript — nothing to digest");
        return Ok(());
    }

    let corrections = extract_corrections(&entries);
    let reinforcements = extract_reinforcements(&entries);
    let files_touched = extract_files_touched(&entries);
    let summary = build_summary(&entries, &corrections);

    info!(
        "digest: {} turns, {} corrections, {} reinforcements, {} files",
        entries.len(), corrections.len(), reinforcements.len(), files_touched.len()
    );

    if !corrections.is_empty() {
        for c in &corrections {
            info!("digest: correction — {:?}", c.feedback);
        }
    }

    // Build the digest text for embedding
    let embed_text = build_embed_text(&summary, &corrections, &reinforcements);
    debug!("digest: embed text ({} chars): {:?}", embed_text.len(), &embed_text[..embed_text.len().min(120)]);

    // Write Digest node
    let content = serde_json::json!({
        "summary": summary,
        "corrections": corrections.iter().map(|c| serde_json::json!({
            "feedback": c.feedback,
            "context": c.context,
            "sentiment": c.sentiment.to_str(),
        })).collect::<Vec<_>>(),
        "reinforcements": reinforcements.iter().map(|r| serde_json::json!({
            "feedback": r.feedback,
            "context": r.context,
        })).collect::<Vec<_>>(),
        "files_touched": files_touched,
        "turn_count": entries.len(),
    });

    let token_count = estimate_tokens(&embed_text);
    let node = node_repo.insert(
        agent.head_node_id,
        agent.agent_id,
        NodeKind::Digest,
        content,
        token_count,
    ).await?;

    // Embed and store
    let embedder = Embedder::new(&config.ollama_base_url, &config.ollama_embed_model);
    if embedder.health_check().await {
        match embedder.embed(&embed_text).await {
            Ok(vec) => {
                node_repo.set_embedding(node.id, vec).await?;
                info!("digest: node {} embedded and stored", node.id);
            }
            Err(e) => warn!("digest: embed failed: {e}"),
        }
    } else {
        warn!("digest: ollama unavailable — digest stored without embedding");
    }

    agent_repo.update_head(agent.agent_id, node.id, agent.context_tokens).await?;

    // Emit events
    let _ = event_repo.emit(
        EventKind::DigestWritten,
        agent_name,
        Some(agent.agent_id),
        serde_json::json!({
            "node_id": node.id,
            "turns": entries.len(),
            "corrections": corrections.len(),
            "reinforcements": reinforcements.len(),
            "summary": &summary[..summary.len().min(120)],
        }),
    ).await;

    for c in &corrections {
        let _ = event_repo.emit(
            EventKind::CorrectionDetected,
            agent_name,
            Some(agent.agent_id),
            serde_json::json!({
                "feedback": c.feedback,
                "sentiment": c.sentiment.to_str(),
                "context": &c.context[..c.context.len().min(120)],
            }),
        ).await;
    }

    // Print a summary to stdout (appears in terminal, not injected)
    if !corrections.is_empty() || !reinforcements.is_empty() {
        println!("[ygg digest] session recorded");
        for c in &corrections {
            println!("  ✗ {}", c.feedback);
        }
        for r in &reinforcements {
            println!("  ✓ {}", r.feedback);
        }
    }

    Ok(())
}

// ── transcript parsing ────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Turn {
    pub role: String,
    pub text: String,
}

fn parse_transcript(path: &str) -> Vec<Turn> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => { warn!("digest: cannot open transcript: {e}"); return vec![]; }
    };

    let mut turns = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        let Ok(val): Result<serde_json::Value, _> = serde_json::from_str(&line) else { continue };

        // Try multiple transcript formats Claude Code might use
        let role = val.get("role")
            .or_else(|| val.get("message").and_then(|m| m.get("role")))
            .and_then(|r| r.as_str())
            .map(|s| s.to_string());

        let text = extract_text(&val);

        if let (Some(role), Some(text)) = (role, text) {
            if !text.trim().is_empty() {
                turns.push(Turn { role, text });
            }
        }
    }

    turns
}

fn extract_text(val: &serde_json::Value) -> Option<String> {
    // Direct string content
    if let Some(s) = val.get("content").and_then(|c| c.as_str()) {
        return Some(s.to_string());
    }
    // Nested message.content string
    if let Some(s) = val.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
        return Some(s.to_string());
    }
    // content array — join text blocks
    let content_arr = val.get("content")
        .or_else(|| val.get("message").and_then(|m| m.get("content")))?
        .as_array()?;

    let text: String = content_arr.iter()
        .filter_map(|block| {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                block.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if text.is_empty() { None } else { Some(text) }
}

// ── correction / reinforcement detection ─────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Sentiment {
    Correction,
    StrongCorrection,
}

impl Sentiment {
    fn to_str(&self) -> &'static str {
        match self {
            Self::Correction => "correction",
            Self::StrongCorrection => "strong_correction",
        }
    }
}

#[derive(Debug)]
pub struct CorrectionSignal {
    pub feedback: String,   // the user's correction text
    pub context: String,    // what the assistant was doing (prev turn snippet)
    pub sentiment: Sentiment,
}

#[derive(Debug)]
pub struct ReinforcementSignal {
    pub feedback: String,
    pub context: String,
}

// Patterns that indicate the user is correcting Claude
const CORRECTION_STARTS: &[&str] = &[
    "no,", "no.", "no -", "no —", "nope", "nah,",
    "wait,", "wait -", "actually,", "actually -",
    "stop,", "stop.", "don't", "dont",
    "wrong", "incorrect", "that's wrong", "that's not",
    "revert", "undo", "go back",
];

const CORRECTION_CONTAINS: &[&str] = &[
    "don't do that", "don't use", "don't add", "don't create",
    "stop doing", "stop adding", "that's not right", "that's not what",
    "i didn't ask", "i said", "not that", "wrong approach",
    "too many", "too much", "unnecessary", "don't need",
];

const REINFORCEMENT_STARTS: &[&str] = &[
    "yes", "perfect", "exactly", "great", "good", "correct",
    "that's it", "that's right", "nice", "looks good",
    "works", "it works", "that works",
];

fn is_correction(text: &str) -> Option<Sentiment> {
    let lower = text.to_lowercase();
    let trimmed = lower.trim();

    // Strong correction: very short message that is just "no" or "stop"
    if matches!(trimmed, "no" | "no!" | "stop" | "wait" | "wrong" | "nope") {
        return Some(Sentiment::StrongCorrection);
    }

    for pattern in CORRECTION_STARTS {
        if trimmed.starts_with(pattern) {
            return Some(Sentiment::Correction);
        }
    }
    for pattern in CORRECTION_CONTAINS {
        if trimmed.contains(pattern) {
            return Some(Sentiment::Correction);
        }
    }
    None
}

fn is_reinforcement(text: &str) -> bool {
    let lower = text.to_lowercase();
    let trimmed = lower.trim();
    // Must be a short positive message (long messages are tasks, not reinforcement)
    if trimmed.len() > 200 { return false; }
    REINFORCEMENT_STARTS.iter().any(|p| trimmed.starts_with(p))
}

fn extract_corrections(turns: &[Turn]) -> Vec<CorrectionSignal> {
    let mut out = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        if turn.role != "user" { continue; }
        if let Some(sentiment) = is_correction(&turn.text) {
            // Get context from the previous assistant turn
            let context = turns[..i].iter().rev()
                .find(|t| t.role == "assistant")
                .map(|t| {
                    let s = t.text.trim();
                    if s.len() > 200 { format!("{}…", &s[..197]) } else { s.to_string() }
                })
                .unwrap_or_default();

            out.push(CorrectionSignal {
                feedback: turn.text.trim().to_string(),
                context,
                sentiment,
            });
        }
    }
    out
}

fn extract_reinforcements(turns: &[Turn]) -> Vec<ReinforcementSignal> {
    let mut out = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        if turn.role != "user" { continue; }
        if is_reinforcement(&turn.text) {
            let context = turns[..i].iter().rev()
                .find(|t| t.role == "assistant")
                .map(|t| {
                    let s = t.text.trim();
                    if s.len() > 200 { format!("{}…", &s[..197]) } else { s.to_string() }
                })
                .unwrap_or_default();

            out.push(ReinforcementSignal {
                feedback: turn.text.trim().to_string(),
                context,
            });
        }
    }
    out
}

/// Extract file paths from tool calls in the transcript.
fn extract_files_touched(turns: &[Turn]) -> Vec<String> {
    // File paths show up in assistant turns as part of tool call descriptions.
    // Simple heuristic: extract anything that looks like a path from assistant text.
    let mut files = std::collections::HashSet::new();
    let path_re = regex::Regex::new(r#"(?:^|[\s`'"(])(/[^\s`'")\n]{3,})"#).unwrap();

    for turn in turns {
        if turn.role != "assistant" { continue; }
        for cap in path_re.captures_iter(&turn.text) {
            let p = cap[1].trim_end_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-');
            if p.contains('.') || p.starts_with("/workspaces") || p.starts_with("/home") {
                files.insert(p.to_string());
            }
        }
    }

    let mut v: Vec<String> = files.into_iter().collect();
    v.sort();
    v
}

// ── summary builder ───────────────────────────────────────────────────────────

fn build_summary(turns: &[Turn], corrections: &[CorrectionSignal]) -> String {
    let mut parts = Vec::new();

    // First user message = the task
    if let Some(first) = turns.iter().find(|t| t.role == "user") {
        let s = first.text.trim();
        parts.push(if s.len() > 300 { format!("{}…", &s[..297]) } else { s.to_string() });
    }

    // Corrections become negative directives
    for c in corrections {
        parts.push(format!("correction: {}", c.feedback));
    }

    // Last user message (if different from first and not a correction)
    if let Some(last_user) = turns.iter().rev().find(|t| t.role == "user") {
        let is_correction = is_correction(&last_user.text).is_some();
        let is_first = turns.iter().find(|t| t.role == "user")
            .map(|f| f.text == last_user.text).unwrap_or(false);
        if !is_correction && !is_first && last_user.text.len() > 10 {
            let s = last_user.text.trim();
            parts.push(if s.len() > 200 { format!("{}…", &s[..197]) } else { s.to_string() });
        }
    }

    parts.join(" | ")
}

fn build_embed_text(
    summary: &str,
    corrections: &[CorrectionSignal],
    reinforcements: &[ReinforcementSignal],
) -> String {
    let mut parts = vec![summary.to_string()];
    for c in corrections {
        parts.push(format!("avoid: {}", c.feedback));
    }
    for r in reinforcements {
        parts.push(format!("good: {}", r.feedback));
    }
    parts.join(". ")
}

fn estimate_tokens(text: &str) -> i32 {
    (text.len() / 4).max(1) as i32
}
