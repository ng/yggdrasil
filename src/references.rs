//! Retrieval outcome measurement — pairs each `similarity_hit` event with
//! the assistant turn that followed it and emits a `hit_referenced` event
//! when overlap between the injected snippet and the assistant's response
//! exceeds a threshold.
//!
//! Called from the digest pipeline (Stop + PreCompact) so it runs batch
//! over whole sessions rather than on the hot path. See yggdrasil-20.

use std::collections::HashSet;
use std::io::BufRead;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::event::{EventKind, EventRepo};

/// Parsed transcript — one entry per Claude Code turn with role + text + timestamp.
pub struct Turn {
    pub role: String,    // "user" | "assistant"
    pub text: String,
    pub ts: Option<DateTime<Utc>>,
}

/// Parse a Claude Code transcript JSONL into role-typed turns. Robust to the
/// variety of message shapes (content may be a string OR an array of content
/// blocks with `type`/`text` fields).
pub fn parse_transcript_turns(path: &str) -> Vec<Turn> {
    let Ok(file) = std::fs::File::open(path) else { return Vec::new(); };
    let reader = std::io::BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue; };
        let Some(msg_type) = v.get("type").and_then(|t| t.as_str()) else { continue; };
        // Claude Code transcripts use type="user"|"assistant" at the top level,
        // and sometimes nest the message inside v["message"].
        let role = match msg_type {
            "user" => "user",
            "assistant" => "assistant",
            _ => continue,
        };
        let text = extract_message_text(&v).unwrap_or_default();
        if text.trim().is_empty() { continue; }
        let ts = v.get("timestamp").and_then(|t| t.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        out.push(Turn { role: role.to_string(), text, ts });
    }
    out
}

fn extract_message_text(v: &serde_json::Value) -> Option<String> {
    // Prefer message.content if present; fallback to top-level content.
    let msg = v.get("message").unwrap_or(v);
    let content = msg.get("content")?;
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let mut parts = Vec::new();
            for block in arr {
                if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                    parts.push(t.to_string());
                }
            }
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        _ => None,
    }
}

/// Tokenize for Jaccard comparison — lowercase, alphanumeric-only word split,
/// drop common stopwords. Returns a set of tokens.
fn tokenize(text: &str) -> HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "a","an","the","and","or","but","if","then","of","in","on","at","to",
        "for","with","by","as","is","are","was","were","be","been","being",
        "it","its","this","that","these","those","have","has","had","do","does","did",
        "i","you","we","they","he","she","them","me","my","your","our",
        "not","no","yes","so","than","so","such","can","will","would","could",
        "should","there","here","which","what","who","how","why","when",
    ];
    let stopwords: HashSet<&str> = STOPWORDS.iter().copied().collect();
    text.split(|c: char| !c.is_alphanumeric())
        .filter_map(|w| {
            let w = w.to_lowercase();
            if w.len() < 3 || stopwords.contains(w.as_str()) { None } else { Some(w) }
        })
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() { return 0.0; }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 { 0.0 } else { inter as f64 / union as f64 }
}

/// Pair similarity_hit events for this agent/session window with the next
/// assistant turn in the transcript. For each pair, score overlap. If the
/// score exceeds the threshold, emit a hit_referenced event.
///
/// Idempotent-ish: skips similarity_hit events that already have a
/// hit_referenced event pointing at them (checked by event id).
pub async fn score_references(
    pool: &sqlx::PgPool,
    agent_id: Uuid,
    agent_name: &str,
    transcript_path: &str,
) -> Result<ReferenceReport, anyhow::Error> {
    let threshold: f64 = std::env::var("YGG_REFERENCE_THRESHOLD")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(0.12);

    let turns = parse_transcript_turns(transcript_path);
    if turns.is_empty() {
        return Ok(ReferenceReport::default());
    }

    // Pull similarity_hit events for this agent, ordered by created_at.
    // We use a 7-day lookback window to keep the join bounded.
    let rows: Vec<(Uuid, DateTime<Utc>, serde_json::Value)> = sqlx::query_as(
        "SELECT id, created_at, payload FROM events
         WHERE event_kind::text = 'similarity_hit' AND agent_id = $1
           AND created_at > now() - interval '7 days'
         ORDER BY created_at ASC"
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(ReferenceReport::default());
    }

    // Build an already-processed set so we're idempotent across digest runs.
    let already: HashSet<Uuid> = sqlx::query_scalar(
        "SELECT (payload->>'similarity_hit_event_id')::uuid FROM events
         WHERE event_kind::text = 'hit_referenced' AND agent_id = $1
           AND payload->>'similarity_hit_event_id' IS NOT NULL"
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .collect();

    // For each similarity_hit event, find the next assistant turn AFTER
    // the hit's timestamp. Transcripts are in chronological order, so
    // we can walk forward.
    let event_repo = EventRepo::new(pool);
    let mut report = ReferenceReport::default();

    for (hit_id, hit_ts, payload) in &rows {
        if already.contains(hit_id) { continue; }
        let Some(snippet) = payload.get("snippet").and_then(|s| s.as_str()) else { continue; };

        // Find the next assistant turn with ts > hit_ts.
        let assistant = turns.iter().find(|t| {
            t.role == "assistant" && t.ts.map(|ts| ts > *hit_ts).unwrap_or(false)
        });
        let Some(assistant) = assistant else {
            // No paired assistant turn yet; skip and try again next pass.
            continue;
        };

        let hit_tokens = tokenize(snippet);
        let resp_tokens = tokenize(&assistant.text);
        let overlap = jaccard(&hit_tokens, &resp_tokens);
        report.scored += 1;

        if overlap >= threshold {
            report.referenced += 1;
            let source_id = payload.get("source_node_id").and_then(|v| v.as_str()).unwrap_or("");
            let _ = event_repo.emit(
                EventKind::HitReferenced,
                agent_name,
                Some(agent_id),
                serde_json::json!({
                    "similarity_hit_event_id": hit_id,
                    "source_node_id": source_id,
                    "overlap": overlap,
                    "method": "jaccard",
                    "threshold": threshold,
                }),
            ).await;
        }
    }

    Ok(report)
}

#[derive(Debug, Default, Clone)]
pub struct ReferenceReport {
    pub scored: u32,
    pub referenced: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_drops_stopwords_and_short() {
        let t = tokenize("The quick brown fox");
        assert!(t.contains("quick"));
        assert!(t.contains("brown"));
        assert!(t.contains("fox"));
        assert!(!t.contains("the"));
    }

    #[test]
    fn jaccard_overlap() {
        let a = tokenize("migration ordering sqlx");
        let b = tokenize("sqlx migration must preserve ordering");
        let j = jaccard(&a, &b);
        assert!(j > 0.5, "expected strong overlap, got {j}");
    }

    #[test]
    fn jaccard_disjoint() {
        let a = tokenize("cat dog fish");
        let b = tokenize("rocket planet star");
        assert_eq!(jaccard(&a, &b), 0.0);
    }
}
