use sqlx::PgPool;
use crate::config::AppConfig;

pub async fn execute(pool: &PgPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    crate::tui::app::run(pool, config).await
}
