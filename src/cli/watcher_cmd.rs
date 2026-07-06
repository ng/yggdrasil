use crate::config::AppConfig;
use crate::watcher::Watcher;
use sqlx::PgPool;

pub async fn execute(pool: &PgPool, config: &AppConfig, once: bool) -> Result<(), anyhow::Error> {
    let watcher = Watcher::new(pool.clone(), config.clone());
    if once {
        // Single opportunistic tick; no-op if a daemon holds the lock.
        watcher.run_once().await?;
        Ok(())
    } else {
        watcher.run().await
    }
}
