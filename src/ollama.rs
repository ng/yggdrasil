use pgvector::Vector;
use serde::{Deserialize, Serialize};

/// Unified Ollama client for both embeddings and chat completions.
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
    embed_model: String,
    chat_model: String,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatMessage,
}

impl OllamaClient {
    pub fn new(base_url: &str, embed_model: &str, chat_model: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            embed_model: embed_model.to_string(),
            chat_model: chat_model.to_string(),
        }
    }

    /// Generate embedding for text. Returns a pgvector-compatible Vector.
    pub async fn embed(&self, text: &str) -> Result<Vector, crate::YggError> {
        let resp = self
            .http
            .post(format!("{}/api/embed", self.base_url))
            .json(&EmbedRequest {
                model: &self.embed_model,
                input: text,
            })
            .send()
            .await
            .map_err(|e| crate::YggError::Ollama(format!("embed request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::YggError::Ollama(format!("embed {status}: {body}")));
        }

        let data: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| crate::YggError::Ollama(format!("embed parse error: {e}")))?;

        let vec = data
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| crate::YggError::Ollama("no embedding returned".into()))?;

        Ok(Vector::from(vec))
    }

    /// Chat completion via local model. Non-streaming.
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<String, crate::YggError> {
        let resp = self
            .http
            .post(format!("{}/api/chat", self.base_url))
            .json(&ChatRequest {
                model: &self.chat_model,
                messages,
                stream: false,
            })
            .send()
            .await
            .map_err(|e| crate::YggError::Ollama(format!("chat request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::YggError::Ollama(format!("chat {status}: {body}")));
        }

        let data: ChatResponse = resp
            .json()
            .await
            .map_err(|e| crate::YggError::Ollama(format!("chat parse error: {e}")))?;

        Ok(data.message.content)
    }

    /// Generate a context digest (snapshot summary) from a block of text.
    pub async fn generate_digest(&self, context_text: &str) -> Result<String, crate::YggError> {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "You are a precise summarizer. Compress the following conversation into a dense summary. Preserve all key decisions, code changes, file paths, and open tasks. Output JSON with fields: summary, active_files, open_tasks, key_decisions.".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: context_text.to_string(),
            },
        ];
        self.chat(&messages).await
    }

    /// Check if Ollama is reachable.
    pub async fn health_check(&self) -> Result<bool, crate::YggError> {
        let resp = self
            .http
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map_err(|e| crate::YggError::Ollama(format!("health check failed: {e}")))?;

        Ok(resp.status().is_success())
    }

    /// Pull a model (used by `ygg init`).
    pub async fn pull_model(&self, model: &str) -> Result<(), crate::YggError> {
        let resp = self
            .http
            .post(format!("{}/api/pull", self.base_url))
            .json(&serde_json::json!({ "name": model, "stream": false }))
            .send()
            .await
            .map_err(|e| crate::YggError::Ollama(format!("pull request failed: {e}")))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::YggError::Ollama(format!("pull failed: {body}")));
        }

        Ok(())
    }
}
