use pgvector::Vector;
use reqwest;
use serde::{Deserialize, Serialize};

/// Embedder using Ollama's /api/embed endpoint.
/// Uses all-minilm model (384d) — pull with: ollama pull all-minilm
pub struct Embedder {
    http: reqwest::Client,
    base_url: String,
    model: String,
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

impl Embedder {
    pub fn new(base_url: &str, model: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
        }
    }

    /// Default embedder using standard Ollama config.
    pub fn default_ollama() -> Self {
        let base_url = std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".into());
        let model = std::env::var("OLLAMA_EMBED_MODEL")
            .unwrap_or_else(|_| "all-minilm".into());
        Self::new(&base_url, &model)
    }

    /// Generate embedding for a single text string.
    pub async fn embed(&self, text: &str) -> Result<Vector, crate::YggError> {
        let resp = self
            .http
            .post(format!("{}/api/embed", self.base_url))
            .json(&EmbedRequest {
                model: &self.model,
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

    /// Check if the embedding service is reachable.
    pub async fn health_check(&self) -> bool {
        self.http
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    /// Embed and store on a node (fire-and-forget).
    pub fn embed_and_store(
        &self,
        node_id: uuid::Uuid,
        text: String,
        pool: sqlx::PgPool,
    ) {
        let base_url = self.base_url.clone();
        let model = self.model.clone();

        tokio::spawn(async move {
            let embedder = Embedder::new(&base_url, &model);
            if let Ok(vec) = embedder.embed(&text).await {
                let repo = crate::models::node::NodeRepo::new(&pool);
                let _ = repo.set_embedding(node_id, vec).await;
            }
        });
    }

    /// Pull the embedding model via Ollama.
    pub async fn pull_model(&self) -> Result<(), crate::YggError> {
        let resp = self
            .http
            .post(format!("{}/api/pull", self.base_url))
            .json(&serde_json::json!({ "name": &self.model, "stream": false }))
            .send()
            .await
            .map_err(|e| crate::YggError::Ollama(format!("pull failed: {e}")))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::YggError::Ollama(format!("pull failed: {body}")));
        }
        Ok(())
    }
}
