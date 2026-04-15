use std::env;

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub database_url: String,
    pub ollama_base_url: String,
    pub ollama_embed_model: String,
    pub ollama_chat_model: String,
    pub embedding_dimensions: usize,
    pub context_limit_tokens: usize,
    pub context_hard_cap_tokens: usize,
    pub lock_ttl_secs: u64,
    pub heartbeat_interval_secs: u64,
    pub watcher_interval_secs: u64,
    pub rtk_binary_path: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, crate::YggError> {
        // Load from ~/.config/ygg/.env first, then local .env as fallback
        if let Ok(home) = env::var("HOME") {
            let config_env = std::path::Path::new(&home).join(".config/ygg/.env");
            if config_env.exists() {
                dotenvy::from_path(&config_env).ok();
            }
        }
        dotenvy::dotenv().ok();

        Ok(Self {
            database_url: env::var("DATABASE_URL")
                .map_err(|_| crate::YggError::Config("DATABASE_URL not set".into()))?,
            ollama_base_url: env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:11434".into()),
            ollama_embed_model: env::var("OLLAMA_EMBED_MODEL")
                .unwrap_or_else(|_| "all-minilm".into()),
            ollama_chat_model: env::var("OLLAMA_CHAT_MODEL")
                .unwrap_or_else(|_| "mistral:7b".into()),
            embedding_dimensions: env::var("EMBEDDING_DIMENSIONS")
                .unwrap_or_else(|_| "384".into())
                .parse()
                .unwrap_or(384),
            context_limit_tokens: env::var("CONTEXT_LIMIT_TOKENS")
                .unwrap_or_else(|_| "250000".into())
                .parse()
                .unwrap_or(250_000),
            context_hard_cap_tokens: env::var("CONTEXT_HARD_CAP_TOKENS")
                .unwrap_or_else(|_| "300000".into())
                .parse()
                .unwrap_or(300_000),
            lock_ttl_secs: env::var("LOCK_TTL_SECS")
                .unwrap_or_else(|_| "300".into())
                .parse()
                .unwrap_or(300),
            heartbeat_interval_secs: env::var("HEARTBEAT_INTERVAL_SECS")
                .unwrap_or_else(|_| "60".into())
                .parse()
                .unwrap_or(60),
            watcher_interval_secs: env::var("WATCHER_INTERVAL_SECS")
                .unwrap_or_else(|_| "30".into())
                .parse()
                .unwrap_or(30),
            rtk_binary_path: env::var("RTK_BINARY_PATH")
                .unwrap_or_else(|_| "rtk".into()),
        })
    }
}
