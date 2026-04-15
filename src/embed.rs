use fastembed::{TextEmbedding, InitOptions, EmbeddingModel};
use pgvector::Vector;
use std::sync::Arc;
use tokio::sync::Mutex;

/// In-process embedding using all-MiniLM-L6-v2 via ONNX runtime.
/// No Ollama required — model is downloaded and cached automatically.
pub struct Embedder {
    model: Arc<Mutex<TextEmbedding>>,
}

impl Embedder {
    /// Initialize the embedding model. Downloads ~30MB on first run,
    /// cached at ~/.cache/huggingface/ after that.
    pub fn new() -> Result<Self, crate::YggError> {
        let mut opts = InitOptions::default();
        opts.model_name = EmbeddingModel::AllMiniLML6V2;
        opts.show_download_progress = true;

        let model = TextEmbedding::try_new(opts)
        .map_err(|e| crate::YggError::Ollama(format!("embedding model init failed: {e}")))?;

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }

    /// Generate embedding for a single text string.
    pub async fn embed(&self, text: &str) -> Result<Vector, crate::YggError> {
        let model = self.model.clone();
        let text = text.to_string();

        // Run in blocking thread — ONNX inference is CPU-bound
        let vec = tokio::task::spawn_blocking(move || {
            let model = model.blocking_lock();
            model.embed(vec![&text], None)
        })
        .await
        .map_err(|e| crate::YggError::Ollama(format!("embed task failed: {e}")))?
        .map_err(|e| crate::YggError::Ollama(format!("embed failed: {e}")))?;

        let embedding = vec.into_iter().next()
            .ok_or_else(|| crate::YggError::Ollama("no embedding returned".into()))?;

        Ok(Vector::from(embedding))
    }

    /// Embed and store on a node (fire-and-forget).
    pub fn embed_and_store(
        &self,
        node_id: uuid::Uuid,
        text: String,
        pool: sqlx::PgPool,
    ) {
        let model = self.model.clone();

        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let model = model.blocking_lock();
                model.embed(vec![&text], None)
            })
            .await;

            if let Ok(Ok(vecs)) = result {
                if let Some(embedding) = vecs.into_iter().next() {
                    let vec = Vector::from(embedding);
                    let repo = crate::models::node::NodeRepo::new(&pool);
                    let _ = repo.set_embedding(node_id, vec).await;
                }
            }
        });
    }
}
