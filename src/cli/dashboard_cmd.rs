use crate::config::AppConfig;
use sqlx::PgPool;

pub async fn execute(pool: &PgPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    crate::tui::app::run(pool, config).await
}
