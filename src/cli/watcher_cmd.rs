use crate::config::AppConfig;
use crate::watcher::Watcher;
use sqlx::PgPool;

pub async fn execute(pool: &PgPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    let watcher = Watcher::new(pool.clone(), config.clone());
    watcher.run().await
}
