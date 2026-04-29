use pgvector::Vector;
use reqwest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Embedder using Ollama's /api/embed endpoint.
/// Default: embeddinggemma (768d native).
pub struct Embedder {
    http: reqwest::Client,
    base_url: String,
    model: String,
    dimensions: usize,
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
        let dimensions: usize = std::env::var("EMBEDDING_DIMENSIONS")
            .unwrap_or_else(|_| "768".into())
            .parse()
            .unwrap_or(768);
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            dimensions,
        }
    }

    /// Default embedder using standard Ollama config.
    pub fn default_ollama() -> Self {
        let base_url =
            std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into());
        let model =
            std::env::var("OLLAMA_EMBED_MODEL").unwrap_or_else(|_| "embeddinggemma".into());
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

        let mut vec = data
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| crate::YggError::Ollama("no embedding returned".into()))?;

        // Truncate if the model returns more dims than the schema expects.
        if vec.len() > self.dimensions {
            vec.truncate(self.dimensions);
        }

        Ok(Vector::from(vec))
    }

    /// Embed with a Postgres-backed content-addressable cache. Returns the
    /// vector and a flag indicating whether it was a cache hit (true) or a
    /// fresh Ollama call (false). On cache errors the call falls through to
    /// a plain embed — caching is a performance optimization, never a
    /// correctness requirement.
    pub async fn embed_cached(
        &self,
        pool: &sqlx::PgPool,
        text: &str,
    ) -> Result<(Vector, bool), crate::YggError> {
        let hash = Sha256::digest(text.as_bytes()).to_vec();

        // Cache lookup. Any error here (schema drift, DB down) means we
        // skip the cache and hit Ollama.
        if let Ok(row) = sqlx::query_as::<_, (Vector,)>(
            "SELECT embedding FROM embedding_cache WHERE content_hash = $1 AND model = $2",
        )
        .bind(&hash)
        .bind(&self.model)
        .fetch_optional(pool)
        .await
        {
            if let Some((vec,)) = row {
                let _ = sqlx::query(
                    "UPDATE embedding_cache SET hit_count = hit_count + 1, last_hit_at = now()
                     WHERE content_hash = $1 AND model = $2",
                )
                .bind(&hash)
                .bind(&self.model)
                .execute(pool)
                .await;
                return Ok((vec, true));
            }
        }

        // Miss → embed, then store.
        let vec = self.embed(text).await?;

        let _ = sqlx::query(
            "INSERT INTO embedding_cache (content_hash, model, embedding)
             VALUES ($1, $2, $3)
             ON CONFLICT (content_hash, model) DO NOTHING",
        )
        .bind(&hash)
        .bind(&self.model)
        .bind(&vec)
        .execute(pool)
        .await;

        Ok((vec, false))
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
    pub fn embed_and_store(&self, node_id: uuid::Uuid, text: String, pool: sqlx::PgPool) {
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
