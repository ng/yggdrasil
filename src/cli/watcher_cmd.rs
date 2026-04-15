use sqlx::PgPool;
use crate::config::AppConfig;
use crate::watcher::Watcher;

pub async fn execute(pool: &PgPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    let watcher = Watcher::new(pool.clone(), config.clone());
    watcher.run().await
}
