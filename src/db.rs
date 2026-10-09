use std::collections::HashSet;
use std::sync::OnceLock;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod initialize;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod package;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod provision;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod runtime;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod supervisor;

pub mod external;
pub mod singleton;

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
        .connect_with(external::options(database_url)?)
        .await
}

/// Central application connection path. Managed startup only opens an existing
/// cluster: it never downloads binaries, initializes data or applies migrations.
pub async fn connect(target: &crate::config::database::DatabaseTarget) -> anyhow::Result<PgPool> {
    use crate::config::database::DatabaseTarget;
    match target {
        DatabaseTarget::External { url } => {
            use anyhow::Context;
            create_pool(url)
                .await
                .context("external database connection failed")
        }
        DatabaseTarget::ManagedLocal { data_dir } => {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                use anyhow::Context;
                let cluster = runtime::ManagedCluster::open(&data_dir.join("postgres"))
                    .context("managed database is not initialized; run ygg init")?;
                supervisor::start(
                    &cluster,
                    &std::env::current_exe()?,
                    std::time::Duration::from_secs(30),
                )
                .await?;
                let max_connections = std::env::var("YGG_DB_POOL")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_MAX_CONNECTIONS);
                Ok(PgPoolOptions::new()
                    .max_connections(max_connections)
                    .connect_with(provision::runtime_options(&cluster))
                    .await
                    .context("managed runtime database unavailable; run ygg migrate explicitly")?)
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                let _ = data_dir;
                anyhow::bail!(
                    "managed database unsupported on this platform; configure an external database"
                )
            }
        }
    }
}

/// Operator-only migration path. Ordinary pools always retain runtime identity.
pub async fn migrate_target(
    target: &crate::config::database::DatabaseTarget,
    owner: Option<&crate::config::database::MigrationOwnerUrl>,
) -> anyhow::Result<()> {
    use crate::config::database::DatabaseTarget;
    match target {
        DatabaseTarget::External { url } => {
            let selected = owner.map(|owner| owner.as_str()).unwrap_or(url);
            external::validate_owner_target(url, selected)?;
            let pool = create_pool(selected).await?;
            let result = run_migrations(&pool).await;
            pool.close().await;
            result?;
        }
        DatabaseTarget::ManagedLocal { data_dir } => {
            anyhow::ensure!(
                owner.is_none(),
                "managed mode cannot use an external owner URL"
            );
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                let cluster = runtime::ManagedCluster::open(&data_dir.join("postgres"))?;
                supervisor::start(
                    &cluster,
                    &std::env::current_exe()?,
                    std::time::Duration::from_secs(30),
                )
                .await?;
                provision::migrate(&cluster).await?;
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                let _ = data_dir;
                anyhow::bail!("managed database unsupported on this platform");
            }
        }
    }
    Ok(())
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

    let applied: HashSet<i64> = match sqlx::query_scalar::<_, i64>(
        "SELECT version FROM _sqlx_migrations WHERE success = true",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows.into_iter().collect(),
        // 42P01 = undefined_table — fresh DB, no migrations applied yet.
        Err(sqlx::Error::Database(ref e)) if e.code().as_deref() == Some("42P01") => HashSet::new(),
        Err(e) => return Err(e.into()),
    };

    let pending: Vec<String> = migrator
        .migrations
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .map(|m| m.description.to_string())
        .collect();
    Ok(pending)
}

/// Explicit installation entry point. External targets never invoke any managed
/// lifecycle or create databases/roles. The supplied URL needs migration rights
/// only when migrations were requested.
pub async fn initialize_target(
    target: &crate::config::database::DatabaseTarget,
    archive: Option<&std::path::Path>,
    migrations: bool,
    owner: Option<&crate::config::database::MigrationOwnerUrl>,
) -> anyhow::Result<()> {
    use crate::config::database::DatabaseTarget;
    match target {
        DatabaseTarget::External { .. } => {
            anyhow::ensure!(
                archive.is_none(),
                "offline PostgreSQL archive requires managed mode"
            );
            if migrations {
                migrate_target(target, owner).await?;
            } else {
                let pool = connect(target).await?;
                sqlx::query("SELECT 1").execute(&pool).await?;
                pool.close().await;
            }
        }
        DatabaseTarget::ManagedLocal { data_dir } => {
            anyhow::ensure!(
                owner.is_none(),
                "managed mode cannot use an external owner URL"
            );
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            initialize::run(data_dir, archive, migrations).await?;
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                let _ = data_dir;
                anyhow::bail!("managed installation unsupported; configure an external database");
            }
        }
    }
    Ok(())
}
