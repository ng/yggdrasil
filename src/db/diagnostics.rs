//! Read-only deployment observations. Stable backend samples cannot certify a
//! pooler's session guarantees; that remains a deployment requirement.
use serde::Serialize;
use sqlx::{Connection, PgConnection, Row, postgres::PgConnectOptions};
use std::time::Duration;

#[derive(Debug, Serialize)]
pub struct DiagnosticReport {
    pub server_major: i32,
    pub tested_major: bool,
    pub uuid_ossp_installed: bool,
    /// This is the PostgreSQL backend transport, which may be behind a proxy.
    pub postgres_backend_tls: bool,
    pub runtime_superuser: bool,
    pub runtime_create_role: bool,
    pub runtime_create_database: bool,
    pub runtime_create_schema: bool,
    pub runtime_create_public_objects: bool,
    pub session: SessionObservation,
    pub session_requirement: &'static str,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionObservation {
    StableButUnverified,
    BackendChangedIncompatible,
}

impl SessionObservation {
    fn from_samples(samples: &[i32]) -> Self {
        if samples.windows(2).any(|pair| pair[0] != pair[1]) {
            Self::BackendChangedIncompatible
        } else {
            Self::StableButUnverified
        }
    }
}

impl DiagnosticReport {
    pub fn observed_incompatibility(&self) -> bool {
        self.session == SessionObservation::BackendChangedIncompatible
    }
}

/// Only SELECTs on a dedicated connection; no advisory locks or session state
/// are left on pooled backends. Never echo an untrusted server error or URL.
pub async fn inspect(options: &PgConnectOptions) -> anyhow::Result<DiagnosticReport> {
    tokio::time::timeout(Duration::from_secs(10), inspect_inner(options))
        .await
        .map_err(|_| anyhow::anyhow!("database diagnostic timed out"))?
}

async fn inspect_inner(options: &PgConnectOptions) -> anyhow::Result<DiagnosticReport> {
    let mut connection = PgConnection::connect_with(options).await.map_err(|_| {
        anyhow::anyhow!(
            "database diagnostic connection failed; check reachability, runtime credentials and TLS certificate/hostname configuration"
        )
    })?;
    let row = sqlx::raw_sql(
        "SELECT current_setting('server_version_num')::int / 10000 AS major,
         EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'uuid-ossp') AS extension,
         COALESCE((SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()), false) AS tls,
         rolsuper, rolcreaterole, rolcreatedb,
         has_database_privilege(current_database(), 'CREATE') AS create_schema,
         has_schema_privilege('public', 'CREATE') AS create_public
         FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(&mut connection)
    .await
    .map_err(|_| anyhow::anyhow!("database diagnostic metadata query failed"))?;
    let mut samples = Vec::new();
    for _ in 0..5 {
        // Simple protocol avoids adding a prepared-statement requirement to
        // this probe. Each SELECT completes its own implicit transaction.
        let sample = sqlx::raw_sql("SELECT pg_backend_pid() AS pid")
            .fetch_one(&mut connection)
            .await
            .map_err(|_| anyhow::anyhow!("database diagnostic session probe failed"))?;
        samples.push(sample.try_get::<i32, _>("pid")?);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    connection
        .close()
        .await
        .map_err(|_| anyhow::anyhow!("database diagnostic connection did not close normally"))?;
    let server_major = row.try_get("major")?;
    Ok(DiagnosticReport {
        server_major,
        tested_major: matches!(server_major, 16 | 18),
        uuid_ossp_installed: row.try_get("extension")?,
        postgres_backend_tls: row.try_get("tls")?,
        runtime_superuser: row.try_get("rolsuper")?,
        runtime_create_role: row.try_get("rolcreaterole")?,
        runtime_create_database: row.try_get("rolcreatedb")?,
        runtime_create_schema: row.try_get("create_schema")?,
        runtime_create_public_objects: row.try_get("create_public")?,
        session: SessionObservation::from_samples(&samples),
        session_requirement: "Scheduler/watcher require a direct or session-preserving endpoint. Transaction pooling is unsupported. Stable samples do not prove compatibility; verify the endpoint configuration with its operator.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_changes_reject_and_stable_samples_never_certify_session_support() {
        assert_eq!(
            SessionObservation::from_samples(&[4, 4, 9, 4]),
            SessionObservation::BackendChangedIncompatible
        );
        assert_eq!(
            SessionObservation::from_samples(&[4; 5]),
            SessionObservation::StableButUnverified
        );
    }
}

/// Operator-only live client audit; managed bootstrap credentials remain inside
/// the database layer and are used only for a read-only transaction. No startup.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub async fn clients(
    config: &crate::config::AppConfig,
) -> anyhow::Result<crate::knowledge::clients::Audit> {
    use crate::config::database::DatabaseTarget;
    use sqlx::{Connection, PgConnection};
    let options = match &config.database {
        DatabaseTarget::External { url } => {
            let selected = config
                .owner_url
                .as_ref()
                .map(|url| url.as_str())
                .unwrap_or(url);
            crate::db::external::validate_owner_target(url, selected)?;
            crate::db::external::options(selected)?
        }
        DatabaseTarget::ManagedLocal { data_dir } => {
            crate::db::runtime::ManagedCluster::open(&data_dir.join("postgres"))?
                .admin_options()
                .database("ygg")
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut connection = PgConnection::connect_with(&options).await.map_err(|_| {
            anyhow::anyhow!(
                "client audit connection failed; verify operator credentials and endpoint"
            )
        })?;
        let mut tx = connection.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let report = crate::knowledge::clients::audit(&mut tx).await?;
        tx.commit().await?;
        Ok::<_, anyhow::Error>(report)
    })
    .await
    .map_err(|_| anyhow::anyhow!("client audit timed out; no compatibility conclusion"))?
}
