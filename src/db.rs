use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const DEFAULT_MAX_CONNECTIONS: u32 = 32;

pub async fn create_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let max_connections: u32 = std::env::var("YGG_DB_POOL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_CONNECTIONS);
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}

pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}
