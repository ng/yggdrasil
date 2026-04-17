use std::io::BufRead;
use std::path::PathBuf;

use crate::config::AppConfig;
use crate::embed::Embedder;
use crate::models::agent::{AgentRepo, AgentState};
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

    let heuristic_corrections = extract_corrections(&entries);
    let heuristic_reinforcements = extract_reinforcements(&entries);
    let files_touched = extract_files_touched(&entries);

    // LLM digest (yggdrasil-4). Asks Ollama ONLY for summary + open_threads
    // — the two fields where a narrative model earns its keep. Corrections
    // and reinforcements stay on the heuristic path; pattern-matching on
    // "no/stop/actually/..." is deterministic and doesn't produce the
    // generic placeholder garbage ("what the user corrected") that a 1B
    // model emits when asked for structured judgments.
    let turn_pairs: Vec<(String, String)> = entries.iter()
        .map(|t| (t.role.clone(), t.text.clone()))
        .collect();
    let llm_result = crate::llm_digest::LlmDigester::from_env().digest(&turn_pairs).await;
    let method = if llm_result.is_some() { "llm" } else { "heuristic" };

    // Summary: LLM if it produced a non-placeholder sentence; else heuristic.
    let summary = llm_result.as_ref()
        .map(|r| r.summary.clone())
        .unwrap_or_else(|| build_summary(&entries, &heuristic_corrections));

    // Corrections + reinforcements: always heuristic. Regex is the right
    // tool here; the LLM demonstrably emits placeholders at 1B.
    let corrections_json: Vec<serde_json::Value> = heuristic_corrections.iter()
        .map(|c| serde_json::json!({
            "feedback": c.feedback, "context": c.context, "sentiment": c.sentiment.to_str(),
        })).collect();
    let reinforcements_json: Vec<serde_json::Value> = heuristic_reinforcements.iter()
        .map(|r| serde_json::json!({
            "feedback": r.feedback, "context": r.context,
        })).collect();

    // Open threads: LLM-only. Heuristic has no equivalent.
    let open_threads: Vec<String> = llm_result.as_ref()
        .map(|r| r.open_threads.clone())
        .unwrap_or_default();

    info!(
        "digest [{method}]: {} turns, {} corrections, {} reinforcements, {} open_threads, {} files",
        entries.len(), corrections_json.len(), reinforcements_json.len(),
        open_threads.len(), files_touched.len()
    );

    // Build the text we'll embed for cross-session similarity retrieval.
    // Pull the canonical forms so the embedding reflects whichever path
    // produced them.
    let embed_text = {
        let mut parts = vec![summary.clone()];
        for c in &corrections_json {
            if let Some(f) = c.get("feedback").and_then(|v| v.as_str()) {
                parts.push(format!("avoid: {f}"));
            }
        }
        for r in &reinforcements_json {
            if let Some(f) = r.get("feedback").and_then(|v| v.as_str()) {
                parts.push(format!("good: {f}"));
            }
        }
        for t in &open_threads { parts.push(format!("open: {t}")); }
        parts.join(". ")
    };
    debug!("digest: embed text ({} chars): {:?}", embed_text.len(), &embed_text[..embed_text.len().min(120)]);

    // Write Digest node
    let content = serde_json::json!({
        "summary": summary,
        "corrections": corrections_json,
        "reinforcements": reinforcements_json,
        "open_threads": open_threads,
        "files_touched": files_touched,
        "turn_count": entries.len(),
        "method": method,
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
        let embed_start = std::time::Instant::now();
        let embed_result = embedder.embed(&embed_text).await;
        let embed_ms = embed_start.elapsed().as_millis() as u64;

        let _ = event_repo.emit(
            EventKind::EmbeddingCall,
            agent_name,
            Some(agent.agent_id),
            serde_json::json!({
                "model": &config.ollama_embed_model,
                "input_chars": embed_text.len(),
                "latency_ms": embed_ms,
                "success": embed_result.is_ok(),
                "purpose": "digest_embed",
            }),
        ).await;

        match embed_result {
            Ok(vec) => {
                node_repo.set_embedding(node.id, vec).await?;
                info!("digest: node {} embedded in {embed_ms}ms", node.id);
            }
            Err(e) => warn!("digest: embed failed ({embed_ms}ms): {e}"),
        }
    } else {
        warn!("digest: ollama unavailable — digest stored without embedding");
    }

    agent_repo.update_head(agent.agent_id, node.id, agent.context_tokens).await?;

    // Score retrieval references — pair similarity_hit events with the
    // assistant turns that followed them, emit hit_referenced events for
    // overlaps ≥ threshold. Batch, idempotent. yggdrasil-20.
    match crate::references::score_references(pool, agent.agent_id, agent_name, transcript_path).await {
        Ok(r) if r.scored > 0 => {
            info!(
                "digest: references scored {} pairs, {} referenced ({:.0}%)",
                r.scored, r.referenced,
                r.referenced as f64 / r.scored as f64 * 100.0
            );
        }
        Ok(_) => {}
        Err(e) => warn!("digest: reference scoring failed: {e}"),
    }

    // Emit events
    let _ = event_repo.emit(
        EventKind::DigestWritten,
        agent_name,
        Some(agent.agent_id),
        serde_json::json!({
            "node_id": node.id,
            "turns": entries.len(),
            "corrections": corrections_json.len(),
            "reinforcements": reinforcements_json.len(),
            "method": method,
            "summary": &summary[..summary.len().min(120)],
        }),
    ).await;

    // Heuristic corrections still feed CorrectionDetected events (they carry
    // sentiment info the LLM path doesn't produce). If the LLM path won, the
    // heuristic array may still have useful entries — emit them regardless.
    for c in &heuristic_corrections {
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

    // Digest runs at Stop (session ended) and PreCompact (about to compact).
    // Either way the agent is no longer actively working — land in Idle.
    if let Err(e) = agent_repo.force_state(agent.agent_id, AgentState::Idle, None).await {
        warn!("digest: force_state failed: {e}");
    }

    // Print a summary to stdout (appears in terminal, not injected)
    if !corrections_json.is_empty() || !reinforcements_json.is_empty() || !open_threads.is_empty() {
        println!("[ygg digest] session recorded [{method}]");
        for c in &corrections_json {
            if let Some(f) = c.get("feedback").and_then(|v| v.as_str()) {
                println!("  ✗ {f}");
            }
        }
        for r in &reinforcements_json {
            if let Some(f) = r.get("feedback").and_then(|v| v.as_str()) {
                println!("  ✓ {f}");
            }
        }
        for t in &open_threads {
            println!("  … {t}");
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

fn estimate_tokens(text: &str) -> i32 {
    (text.len() / 4).max(1) as i32
}

/// Locate the most recently modified Claude Code transcript for the current
/// working directory's session. Used by `ygg digest --now` so the user doesn't
/// have to pass `--transcript` explicitly mid-session.
///
/// Claude Code stores transcripts under `~/.claude/projects/<slug>/<session>.jsonl`
/// where the slug is a munged version of the project path (e.g.
/// `/Users/ng/Documents/GitHub/yggdrasil` → `-Users-ng-Documents-GitHub-yggdrasil`).
pub fn find_latest_transcript() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let slug = cwd.to_string_lossy().replace('/', "-");
    let home = std::env::var("HOME").ok()?;
    let project_dir = PathBuf::from(&home).join(".claude/projects").join(&slug);
    if !project_dir.exists() {
        return None;
    }
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&project_dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") { continue; }
        let mtime = entry.metadata().ok().and_then(|m| m.modified().ok())?;
        match &newest {
            None => newest = Some((mtime, p)),
            Some((t, _)) if mtime > *t => newest = Some((mtime, p)),
            _ => {}
        }
    }
    newest.map(|(_, p)| p.to_string_lossy().to_string())
}
