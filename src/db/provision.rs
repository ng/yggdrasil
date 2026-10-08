//! Explicit managed-only schema provisioning. Local trust authentication is
//! bounded by the private, same-OS-user socket; role separation limits ordinary
//! SQL mistakes, not an adversary with access to that user's files/socket.
use anyhow::{Result, ensure};
use sqlx::{Connection, PgConnection, Row, postgres::PgConnectOptions};

use super::runtime::ManagedCluster;

const OWNER: &str = "ygg_owner";
const RUNTIME: &str = "ygg_runtime";
const DATABASE: &str = "ygg";
const TABLES: &[&str] = &[
    "locks",
    "agents",
    "agent_stats",
    "events",
    "repos",
    "sessions",
    "tasks",
    "task_deps",
    "task_labels",
    "task_events",
    "task_seq",
    "task_links",
    "session_summaries",
    "workers",
    "task_runs",
    "learnings",
    "bench_runs",
    "bench_task_results",
    "bench_metrics",
    "memories",
    "handoffs",
    "knowledge_usage",
    "knowledge_applications",
];

/// Does not start, create, download, or migrate anything. Callers must establish
/// managed readiness first. Never expose bootstrap credentials to normal pools.
pub fn runtime_options(cluster: &ManagedCluster) -> PgConnectOptions {
    cluster
        .admin_options()
        .username(RUNTIME)
        .database(DATABASE)
        .application_name("ygg")
}

/// Explicit init/migrate operation, resumable after each committed step. A
/// dedicated session lock serializes provisioning without weakening the lifetime
/// process-owner lease. Unexpected roles/databases are rejected, never adopted.
pub async fn migrate(cluster: &ManagedCluster) -> Result<()> {
    cluster
        .wait_ready(std::time::Duration::from_secs(10))
        .await?;
    let mut admin = PgConnection::connect_with(&cluster.admin_options()).await?;
    sqlx::query("SET statement_timeout = '30s'")
        .execute(&mut admin)
        .await?;
    sqlx::query("SELECT pg_advisory_lock(1497843531, 2)")
        .execute(&mut admin)
        .await?;
    let marker = format!("ygg managed cluster {}", cluster.id());
    let mut tx = admin.begin().await?;
    for role in [OWNER, RUNTIME] {
        let existing = sqlx::query("SELECT oid, rolsuper, rolcreatedb, rolcreaterole, rolreplication, rolbypassrls, rolcanlogin, shobj_description(oid, 'pg_authid') AS marker FROM pg_roles WHERE rolname = $1")
            .bind(role).fetch_optional(&mut *tx).await?;
        if let Some(row) = existing {
            ensure!(
                row.try_get::<Option<String>, _>("marker")?.as_deref() == Some(&marker),
                "refusing unrelated managed role {role}"
            );
            for attribute in [
                "rolsuper",
                "rolcreatedb",
                "rolcreaterole",
                "rolreplication",
                "rolbypassrls",
            ] {
                ensure!(
                    !row.try_get::<bool, _>(attribute)?,
                    "managed role {role} has unexpected privilege {attribute}"
                );
            }
            ensure!(
                row.try_get::<bool, _>("rolcanlogin")?,
                "managed role {role} cannot log in"
            );
            let membership: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_auth_members WHERE member = (SELECT oid FROM pg_roles WHERE rolname = $1))")
                .bind(role).fetch_one(&mut *tx).await?;
            ensure!(
                !membership,
                "managed role {role} has unexpected role membership"
            );
        } else {
            // Names are fixed constants; marker contains only our UUID.
            sqlx::query(&format!("CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"))
                .execute(&mut *tx).await?;
            sqlx::query(&format!("COMMENT ON ROLE {role} IS '{marker}'"))
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    let owner: Option<String> =
        sqlx::query_scalar("SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = $1")
            .bind(DATABASE)
            .fetch_optional(&mut admin)
            .await?;
    match owner {
        Some(owner) => ensure!(owner == OWNER, "refusing unrelated managed database"),
        None => {
            sqlx::query("CREATE DATABASE ygg OWNER ygg_owner TEMPLATE template0 ENCODING 'UTF8'")
                .execute(&mut admin)
                .await?;
        }
    }
    // These changes precede migrations, so a failed migration cannot make a
    // partially built schema accessible to a fresh runtime role.
    sqlx::query("REVOKE ALL ON DATABASE ygg FROM PUBLIC, ygg_runtime")
        .execute(&mut admin)
        .await?;
    let mut owner =
        PgConnection::connect_with(&cluster.admin_options().username(OWNER).database(DATABASE))
            .await?;
    sqlx::query("REVOKE CREATE ON SCHEMA public FROM PUBLIC, ygg_runtime")
        .execute(&mut owner)
        .await?;
    sqlx::migrate!("./migrations").run(&mut owner).await?;
    let mut tx = owner.begin().await?;
    for table in TABLES {
        sqlx::query(&format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE public.{table} TO ygg_runtime"
        ))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO ygg_runtime")
        .execute(&mut *tx)
        .await?;
    sqlx::query("GRANT USAGE ON SCHEMA public TO ygg_runtime")
        .execute(&mut *tx)
        .await?;
    sqlx::query("REVOKE ALL ON public.knowledge_storage, public._sqlx_migrations FROM ygg_runtime")
        .execute(&mut *tx)
        .await?;
    sqlx::query("GRANT SELECT ON public.knowledge_storage, public._sqlx_migrations TO ygg_runtime")
        .execute(&mut *tx)
        .await?;
    sqlx::query("GRANT CONNECT ON DATABASE ygg TO ygg_runtime")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    owner.close().await?;
    admin.close().await?;
    Ok(())
}
