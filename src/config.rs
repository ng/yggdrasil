pub mod database;

use std::env;

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub database: database::DatabaseTarget,
    pub context_limit_tokens: usize,
    pub context_hard_cap_tokens: usize,
    pub lock_ttl_secs: u64,
    pub heartbeat_interval_secs: u64,
    pub watcher_interval_secs: u64,
    pub rtk_binary_path: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, crate::YggError> {
        // Preserve legacy user .env knobs consumed directly by older modules
        // (YGG_USER, YGG_DB_POOL, scheduler/hook options). Never search cwd.
        let inputs = env::vars().collect();
        let config = Self::from_environment(inputs)?;
        let dir = database::config_dir(&env::vars().collect())?;
        dotenvy::from_path(dir.join(".env")).ok();
        Ok(config)
    }

    /// Resolve all settings from one environment snapshot without process-wide
    /// mutation. Repository-local .env files are never inputs.
    pub fn from_environment(env: database::Environment) -> Result<Self, crate::YggError> {
        let dir = database::config_dir(&env)?;
        let env = database::user_environment(env)?;
        let deployment = database::DeploymentConfig::from_user_environment(&env, &dir)?;
        Ok(Self {
            database: deployment.database,
            context_limit_tokens: env
                .get("CONTEXT_LIMIT_TOKENS")
                .map(String::as_str)
                .unwrap_or("250000")
                .parse()
                .unwrap_or(250_000),
            context_hard_cap_tokens: env
                .get("CONTEXT_HARD_CAP_TOKENS")
                .map(String::as_str)
                .unwrap_or("300000")
                .parse()
                .unwrap_or(300_000),
            lock_ttl_secs: env
                .get("LOCK_TTL_SECS")
                .map(String::as_str)
                .unwrap_or("300")
                .parse()
                .unwrap_or(300),
            heartbeat_interval_secs: env
                .get("HEARTBEAT_INTERVAL_SECS")
                .map(String::as_str)
                .unwrap_or("60")
                .parse()
                .unwrap_or(60),
            watcher_interval_secs: env
                .get("WATCHER_INTERVAL_SECS")
                .map(String::as_str)
                .unwrap_or("30")
                .parse()
                .unwrap_or(30),
            rtk_binary_path: env
                .get("RTK_BINARY_PATH")
                .map(String::as_str)
                .unwrap_or("rtk")
                .into(),
        })
    }
}
