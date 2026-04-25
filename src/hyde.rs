//! HyDE (Hypothetical Document Embeddings) — yggdrasil-5.
//!
//! Has a small chat model write a plausible answer to the user's prompt,
//! then embeds the answer. Used as a second query vector against pgvector:
//! answers cluster tighter with answer-shaped past content than raw
//! questions do. Standard trick from the RAG literature.
//!
//! Default OFF (adds ~500-1500ms per inject on a local CPU Ollama even
//! with keep_alive). Opt-in via YGG_HYDE=on.

use reqwest;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const DEFAULT_MODEL: &str = "llama3.2:1b";
const DEFAULT_TIMEOUT_MS: u64 = 3_000;

pub struct Hyde {
    http: reqwest::Client,
    base_url: String,
    model: String,
    timeout_ms: u64,
    enabled: bool,
}

impl Hyde {
    pub fn from_env() -> Self {
        let enabled = matches!(
            std::env::var("YGG_HYDE").ok().as_deref(),
            Some("on" | "1" | "true")
        );
        let base_url =
            std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into());
        let model = std::env::var("YGG_HYDE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let timeout_ms = std::env::var("YGG_HYDE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            timeout_ms,
            enabled,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Generate a short hypothetical answer to the prompt. Returns None on
    /// any failure — caller uses the raw-prompt embedding instead.
    pub async fn expand(&self, prompt: &str) -> Option<String> {
        if !self.enabled || prompt.trim().is_empty() {
            return None;
        }

        #[derive(Serialize)]
        struct Req<'a> {
            model: &'a str,
            prompt: String,
            stream: bool,
            options: Opts,
            keep_alive: &'a str,
        }
        #[derive(Serialize)]
        struct Opts {
            temperature: f32,
            num_predict: u32,
        }
        #[derive(Deserialize)]
        struct Resp {
            response: String,
        }

        // Short instruction — we want a 1-2 sentence plausible answer, not
        // a full response. Shorter output = faster inference + tighter
        // embedding cluster.
        let instruction = format!(
            "Write a short, plausible 1-2 sentence technical answer to the \
             following question. Use specific jargon. Do NOT refuse or \
             qualify. Answer:\n\n{prompt}\n\nAnswer:"
        );

        let req = Req {
            model: &self.model,
            prompt: instruction,
            stream: false,
            options: Opts {
                temperature: 0.1,
                num_predict: 100,
            },
            keep_alive: "30m",
        };

        let fut = async {
            let resp = self
                .http
                .post(format!("{}/api/generate", self.base_url))
                .json(&req)
                .send()
                .await
                .map_err(|e| format!("request: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("http {}", resp.status()));
            }
            let body: Resp = resp.json().await.map_err(|e| format!("body: {e}"))?;
            Ok::<String, String>(body.response)
        };

        let raw = match tokio::time::timeout(std::time::Duration::from_millis(self.timeout_ms), fut)
            .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                warn!("hyde: {e}");
                return None;
            }
            Err(_) => {
                warn!("hyde: timed out after {}ms", self.timeout_ms);
                return None;
            }
        };

        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() < 10 {
            return None;
        }
        debug!(
            "hyde: expanded '{}' → '{}'",
            &prompt[..prompt.len().min(40)],
            &trimmed[..trimmed.len().min(80)]
        );
        Some(trimmed.to_string())
    }
}
