use std::collections::HashSet;
use std::sync::OnceLock;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const DEFAULT_MAX_CONNECTIONS: u32 = 32;

static USER_ID: OnceLock<String> = OnceLock::new();

/// Return the cached user identity. Resolved once per process.
pub fn user_id() -> &'static str {
    USER_ID.get_or_init(resolve_user)
}

/// Resolve the current user identity.
/// Priority: YGG_USER env → whoami output → "default".
pub fn resolve_user() -> String {
    if let Ok(u) = std::env::var("YGG_USER") {
        if !u.is_empty() {
            return u;
        }
    }
    std::process::Command::new("whoami")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

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

/// Return descriptions of migrations that are compiled into the binary but
/// not yet applied to the database. Returns an empty vec when fully up to date.
/// Gracefully handles the case where `_sqlx_migrations` doesn't exist yet
/// (fresh DB) by treating all migrations as pending.
pub async fn pending_migrations(pool: &PgPool) -> Result<Vec<String>, anyhow::Error> {
    let migrator = sqlx::migrate!("./migrations");

    let applied: HashSet<i64> =
        sqlx::query_scalar::<_, i64>("SELECT version FROM _sqlx_migrations WHERE success = true")
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

    let pending: Vec<String> = migrator
        .migrations
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .map(|m| m.description.to_string())
        .collect();
    Ok(pending)
}
