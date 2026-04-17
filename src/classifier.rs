//! Zero-shot relevance classifier for `ygg inject` candidates.
//!
//! See ADR 0011 for the design. Runs a small chat model via Ollama's
//! /api/generate with structured JSON output to score (user_prompt,
//! candidate_snippet) pairs. Fails open — any error path returns the
//! candidate unfiltered so retrieval never regresses below the cosine-only
//! baseline.

use reqwest;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const DEFAULT_MODEL: &str = "llama3.2:1b";
const DEFAULT_THRESHOLD: f64 = 0.55;
// Ollama serializes inference server-side per model, so parallelism on the
// client doesn't reduce total wall time. 5s gives a 1B model room to handle
// 8 candidates on CPU with a cold-start first call. Override with
// YGG_CLASSIFIER_TIMEOUT_MS if you want harder limits.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;

pub struct Classifier {
    http: reqwest::Client,
    base_url: String,
    model: String,
    threshold: f64,
    enabled: bool,
    timeout_ms: u64,
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: String,
    format: &'static str,
    stream: bool,
    options: GenerateOptions,
    // Keep the model resident in Ollama after the call. Without this Ollama
    // evicts the model after ~5 minutes of inactivity and re-loads on the
    // next call — for llama3.2:1b that's an 8-9s cold start per inject.
    keep_alive: &'a str,
}

#[derive(Serialize)]
struct GenerateOptions {
    temperature: f32,
    num_predict: u32,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub score: f64,
    pub kept: bool,
    pub bypassed: bool,
    pub reason: &'static str,
}

impl Classifier {
    pub fn from_env() -> Self {
        let enabled = std::env::var("YGG_CLASSIFIER")
            .map(|v| v != "off" && v != "0" && v != "false")
            .unwrap_or(true);
        let base_url = std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".into());
        let model = std::env::var("YGG_CLASSIFIER_MODEL")
            .unwrap_or_else(|_| DEFAULT_MODEL.into());
        let threshold = std::env::var("YGG_CLASSIFIER_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_THRESHOLD);
        let timeout_ms = std::env::var("YGG_CLASSIFIER_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            threshold,
            enabled,
            timeout_ms,
        }
    }

    pub fn threshold(&self) -> f64 { self.threshold }
    pub fn model(&self) -> &str { &self.model }
    pub fn is_enabled(&self) -> bool { self.enabled }

    /// Classify a batch in a SINGLE model call. Ollama serializes inference
    /// per model server-side, so parallel per-candidate calls don't reduce
    /// wall time — one batched prompt does. The model is asked to return a
    /// JSON array of scores in the same order as the input memories.
    pub async fn classify_batch(
        &self,
        prompt: &str,
        candidates: &[&str],
    ) -> Vec<Decision> {
        if !self.enabled {
            return candidates.iter().map(|_| bypass("disabled")).collect();
        }
        if candidates.is_empty() {
            return vec![];
        }

        let mut memories_block = String::new();
        for (i, c) in candidates.iter().enumerate() {
            // Short snippet; avoid flooding the prompt.
            let snippet = if c.len() > 240 { &c[..240] } else { c };
            memories_block.push_str(&format!("{}. {}\n", i + 1, snippet));
        }

        let instruction = format!(
            "You are rating which past memories are relevant to the user's current prompt. \
             For each numbered memory, emit a relevance score between 0.0 and 1.0. \
             Respond with JSON only, shape: {{\"scores\": [<n floats>]}}. Exactly {} scores, \
             in the order given.\n\n\
             Current prompt: {prompt}\n\n\
             Memories:\n{memories_block}\nJSON:",
            candidates.len()
        );

        let req = GenerateRequest {
            model: &self.model,
            prompt: instruction,
            format: "json",
            stream: false,
            options: GenerateOptions {
                temperature: 0.0,
                // Enough tokens for a JSON array of k floats with a few chars
                // of overhead. 20 floats ~ 200 tokens of headroom.
                num_predict: 200,
            },
            keep_alive: "30m",
        };

        let fut = async {
            let resp = self.http
                .post(format!("{}/api/generate", self.base_url))
                .json(&req)
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;

            if !resp.status().is_success() {
                return Err(format!("http {}", resp.status()));
            }
            let body: GenerateResponse = resp.json().await
                .map_err(|e| format!("parse: {e}"))?;
            Ok::<String, String>(body.response)
        };

        let timeout_ms = self.timeout_ms;
        let raw = match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            fut,
        ).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                warn!("classifier: {e}");
                return candidates.iter().map(|_| fail_open("request_failed")).collect();
            }
            Err(_) => {
                warn!("classifier: batch timed out ({timeout_ms}ms); bypassing");
                return candidates.iter().map(|_| bypass("timeout")).collect();
            }
        };

        let scores = match parse_scores(&raw, candidates.len()) {
            Some(s) => s,
            None => {
                debug!("classifier: unparseable response '{}'", raw);
                return candidates.iter().map(|_| fail_open("invalid_json")).collect();
            }
        };

        candidates.iter().enumerate().map(|(i, _)| {
            let score = scores.get(i).copied().unwrap_or(1.0).clamp(0.0, 1.0);
            Decision {
                score,
                kept: score >= self.threshold,
                bypassed: false,
                reason: "scored",
            }
        }).collect()
    }
}

/// Accept any of the JSON shapes small models tend to emit for this prompt:
///   {"scores": [0.5, 0.2, 0.9]}
///   [0.5, 0.2, 0.9]
///   {"1": 0.5, "2": 0.2, "3": 0.9}
///   {"scores": {"1": 0.5, ...}}
/// Missing entries default to 1.0 at the caller so unclassified candidates
/// pass through rather than being silently dropped.
fn parse_scores(raw: &str, expected: usize) -> Option<Vec<f64>> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;

    // Shape 1: {"scores": [...]}
    if let Some(arr) = v.get("scores").and_then(|s| s.as_array()) {
        return Some(arr.iter().filter_map(|x| x.as_f64()).collect());
    }
    // Shape 2: bare array
    if let Some(arr) = v.as_array() {
        return Some(arr.iter().filter_map(|x| x.as_f64()).collect());
    }
    // Shape 3: {"1": 0.5, "2": 0.2, ...} (1-indexed keys)
    if let Some(obj) = v.as_object() {
        // 3a: {"scores": {"1": ..., "2": ...}}
        if let Some(inner) = obj.get("scores").and_then(|x| x.as_object()) {
            return Some(extract_numbered(inner, expected));
        }
        // 3b: obj itself is numbered keys
        if obj.keys().any(|k| k.parse::<usize>().is_ok()) {
            return Some(extract_numbered(obj, expected));
        }
    }
    None
}

fn extract_numbered(obj: &serde_json::Map<String, serde_json::Value>, expected: usize) -> Vec<f64> {
    (1..=expected)
        .map(|i| obj.get(&i.to_string()).and_then(|v| v.as_f64()).unwrap_or(1.0))
        .collect()
}

fn fail_open(reason: &'static str) -> Decision {
    Decision { score: 1.0, kept: true, bypassed: true, reason }
}

fn bypass(reason: &'static str) -> Decision {
    Decision { score: 1.0, kept: true, bypassed: true, reason }
}
