//! LLM-generated session digests — see yggdrasil-4.
//!
//! Replaces the heuristic extractor with a single Ollama generate call
//! that asks for structured JSON: summary, corrections, reinforcements,
//! open_threads. Falls back to heuristic extraction when Ollama is
//! unavailable, times out, or emits unparseable JSON.
//!
//! Design (see ADR 0011 for the classifier's analogous structure):
//! - `keep_alive: 30m` so the model stays resident across sessions.
//! - JSON format-mode; lenient parser copes with mild schema variation.
//! - Hard timeout budget so a misbehaving model can't hang Stop.
//! - Kill-switch: `YGG_LLM_DIGEST=off`.

use reqwest;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const DEFAULT_MODEL: &str = "llama3.2:1b";
const DEFAULT_TIMEOUT_MS: u64 = 20_000;  // digest is not on a hot path; give the model room
const MAX_TURNS: usize = 60;              // last N turns fed into the prompt
const MAX_CHARS_PER_TURN: usize = 400;    // truncate each turn snippet

#[derive(Debug, Clone, Default, Serialize)]
pub struct LlmDigestResult {
    pub summary: String,
    pub corrections: Vec<DigestItem>,
    pub reinforcements: Vec<DigestItem>,
    pub open_threads: Vec<String>,
    pub model: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DigestItem {
    pub feedback: String,
    #[serde(default)]
    pub context: String,
}

pub struct LlmDigester {
    http: reqwest::Client,
    base_url: String,
    model: String,
    timeout_ms: u64,
    enabled: bool,
}

impl LlmDigester {
    pub fn from_env() -> Self {
        let enabled = !matches!(
            std::env::var("YGG_LLM_DIGEST").ok().as_deref(),
            Some("off" | "0" | "false")
        );
        let base_url = std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".into());
        let model = std::env::var("YGG_DIGEST_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let timeout_ms = std::env::var("YGG_LLM_DIGEST_TIMEOUT_MS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TIMEOUT_MS);
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            timeout_ms,
            enabled,
        }
    }

    pub fn is_enabled(&self) -> bool { self.enabled }

    /// Run the LLM digest. Returns None on any failure — caller falls back.
    pub async fn digest(&self, turns: &[(String, String)]) -> Option<LlmDigestResult> {
        if !self.enabled || turns.is_empty() {
            return None;
        }

        let transcript = build_transcript_block(turns);
        let instruction = format!(
            "Summarize this Claude Code session. Respond with JSON only, shape:\n\
             {{\n\
               \"summary\": \"one sentence describing what the session accomplished\",\n\
               \"corrections\": [{{\"feedback\": \"what the user corrected\", \"context\": \"the situation\"}}],\n\
               \"reinforcements\": [{{\"feedback\": \"what the user affirmed\", \"context\": \"the situation\"}}],\n\
               \"open_threads\": [\"unresolved questions or follow-ups\"]\n\
             }}\n\
             Be concrete. Skip fields you can't fill (use empty arrays). Max 3 items each.\n\n\
             Transcript:\n{transcript}\n\nJSON:"
        );

        #[derive(Serialize)]
        struct Req<'a> {
            model: &'a str,
            prompt: String,
            format: &'static str,
            stream: bool,
            options: Opts,
            keep_alive: &'a str,
        }
        #[derive(Serialize)]
        struct Opts { temperature: f32, num_predict: u32 }
        #[derive(Deserialize)]
        struct Resp { response: String }

        let req = Req {
            model: &self.model,
            prompt: instruction,
            format: "json",
            stream: false,
            options: Opts { temperature: 0.0, num_predict: 600 },
            keep_alive: "30m",
        };

        let start = std::time::Instant::now();
        let fut = async {
            let resp = self.http.post(format!("{}/api/generate", self.base_url))
                .json(&req).send().await
                .map_err(|e| format!("request: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("http {}", resp.status()));
            }
            let body: Resp = resp.json().await.map_err(|e| format!("body: {e}"))?;
            Ok::<String, String>(body.response)
        };

        let raw = match tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            fut,
        ).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => { warn!("llm_digest: {e}"); return None; }
            Err(_) => { warn!("llm_digest: timed out after {}ms", self.timeout_ms); return None; }
        };

        let latency_ms = start.elapsed().as_millis() as u64;
        debug!("llm_digest: raw response ({}ms): {}", latency_ms, &raw[..raw.len().min(400)]);

        let parsed = parse_digest_json(&raw)?;
        info!(
            "llm_digest: summary={:?} corrections={} reinforcements={} open_threads={} ({latency_ms}ms)",
            &parsed.summary[..parsed.summary.len().min(80)],
            parsed.corrections.len(),
            parsed.reinforcements.len(),
            parsed.open_threads.len(),
        );

        Some(LlmDigestResult {
            summary: parsed.summary,
            corrections: parsed.corrections,
            reinforcements: parsed.reinforcements,
            open_threads: parsed.open_threads,
            model: self.model.clone(),
            latency_ms,
        })
    }
}

fn build_transcript_block(turns: &[(String, String)]) -> String {
    // Take the last N turns; truncate each; label roles.
    let start = turns.len().saturating_sub(MAX_TURNS);
    let mut out = String::new();
    for (role, text) in &turns[start..] {
        let t = if text.chars().count() > MAX_CHARS_PER_TURN {
            let cut = text.char_indices().nth(MAX_CHARS_PER_TURN).map(|(i, _)| i).unwrap_or(text.len());
            format!("{}…", &text[..cut])
        } else {
            text.clone()
        };
        out.push_str(&format!("{role}: {}\n\n", t.trim()));
    }
    out
}

#[derive(Default)]
struct ParsedDigest {
    summary: String,
    corrections: Vec<DigestItem>,
    reinforcements: Vec<DigestItem>,
    open_threads: Vec<String>,
}

fn parse_digest_json(raw: &str) -> Option<ParsedDigest> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let obj = v.as_object()?;
    let mut out = ParsedDigest::default();

    if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
        out.summary = s.to_string();
    }

    // Corrections / reinforcements may be arrays of objects OR arrays of
    // strings. Handle both shapes.
    for (field, target) in [
        ("corrections", &mut out.corrections),
        ("reinforcements", &mut out.reinforcements),
    ] {
        if let Some(arr) = obj.get(field).and_then(|v| v.as_array()) {
            for item in arr {
                match item {
                    serde_json::Value::String(s) => {
                        target.push(DigestItem { feedback: s.clone(), context: String::new() });
                    }
                    serde_json::Value::Object(_) => {
                        if let Ok(di) = serde_json::from_value::<DigestItem>(item.clone()) {
                            if !di.feedback.is_empty() { target.push(di); }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(arr) = obj.get("open_threads").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(s) = item.as_str() { out.open_threads.push(s.to_string()); }
        }
    }

    // Require at least a summary; empty digest is not useful.
    if out.summary.trim().is_empty() { return None; }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_shape() {
        let raw = r#"{"summary":"fixed a bug","corrections":[{"feedback":"use channels","context":"Arc<Mutex>"}],"reinforcements":[],"open_threads":["test coverage"]}"#;
        let p = parse_digest_json(raw).unwrap();
        assert_eq!(p.summary, "fixed a bug");
        assert_eq!(p.corrections.len(), 1);
        assert_eq!(p.corrections[0].feedback, "use channels");
        assert_eq!(p.open_threads, vec!["test coverage"]);
    }

    #[test]
    fn parses_string_array_corrections() {
        // Small models sometimes emit bare strings instead of objects.
        let raw = r#"{"summary":"stuff","corrections":["avoid X","prefer Y"],"reinforcements":[],"open_threads":[]}"#;
        let p = parse_digest_json(raw).unwrap();
        assert_eq!(p.corrections.len(), 2);
        assert_eq!(p.corrections[0].feedback, "avoid X");
    }

    #[test]
    fn rejects_empty_summary() {
        let raw = r#"{"summary":"","corrections":[],"reinforcements":[],"open_threads":[]}"#;
        assert!(parse_digest_json(raw).is_none());
    }

    #[test]
    fn rejects_non_json() {
        assert!(parse_digest_json("not json at all").is_none());
    }
}
