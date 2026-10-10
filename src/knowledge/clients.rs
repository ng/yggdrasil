//! Server-bound compatibility declarations and a live-connection audit. An
//! observed registration is not an offline-host census or pooler certification.
use super::guard::CLIENT_PROTOCOL;
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgConnection, Row};
use std::sync::OnceLock;
use uuid::Uuid;

static PROCESS: OnceLock<Uuid> = OnceLock::new();

/// Bootstrap on each physical application connection. Pre-compatibility schemas
/// remain connectable for explicit migration; those sessions stay unregistered.
/// Once the function exists, any registration failure refuses the connection.
pub async fn register(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    let available: bool = sqlx::query_scalar("SELECT pg_catalog.to_regprocedure('public.ygg_knowledge_register_client(integer,text,uuid,timestamp with time zone)') IS NOT NULL")
        .fetch_one(&mut *connection).await?;
    if available {
        sqlx::query("SELECT public.ygg_knowledge_register_client($1,$2,$3,(SELECT backend_start FROM pg_catalog.pg_stat_activity WHERE pid=pg_backend_pid()))")
            .bind(CLIENT_PROTOCOL)
            .bind(env!("CARGO_PKG_VERSION"))
            .bind(*PROCESS.get_or_init(Uuid::new_v4))
            .execute(connection).await?;
    }
    Ok(())
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    CompatibleRegistration,
    ProtocolTooOld,
    Unregistered,
}

#[derive(Debug, Serialize)]
pub struct Client {
    pub backend_pid: i32,
    pub backend_start: DateTime<Utc>,
    pub role: String,
    pub process_id: Option<Uuid>,
    pub protocol: Option<i32>,
    pub binary_version: Option<String>,
    pub compatibility: Compatibility,
}

#[derive(Debug, Serialize)]
pub struct Audit {
    pub database_id: Uuid,
    pub generation: i64,
    pub minimum_client: i32,
    pub backend: String,
    pub sampled_at: DateTime<Utc>,
    pub clients: Vec<Client>,
    pub live_blockers: usize,
    pub remaining_verification: [&'static str; 3],
}

/// Read-only point-in-time observation on an operator connection. Caller must
/// provide visibility of every backend and SELECT on the protected registry.
/// Queries omit query text, remote addresses and arbitrary application names.
/// The inspecting connection itself is excluded. A transition must recheck under
/// its migration lease and separately account for offline hosts/external editors.
pub async fn audit(connection: &mut PgConnection) -> Result<Audit> {
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut *connection)
        .await?;
    ensure!(
        isolation == "read committed",
        "client audit requires READ COMMITTED to detect generation changes"
    );
    let visible: bool = sqlx::query_scalar("SELECT pg_catalog.pg_has_role(current_user,'pg_read_all_stats','USAGE') OR (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname=current_user)")
        .fetch_one(&mut *connection).await?;
    ensure!(
        visible,
        "client audit requires pg_read_all_stats or superuser visibility; partial statistics cannot certify live clients"
    );
    // Clear a prior statistics snapshot if a caller reused its transaction.
    sqlx::query("SELECT pg_catalog.pg_stat_clear_snapshot()")
        .execute(&mut *connection)
        .await?;
    let marker: (Uuid, i64, i32, String, DateTime<Utc>) = sqlx::query_as("SELECT database_id,generation,minimum_client,backend,clock_timestamp() FROM public.knowledge_storage WHERE singleton")
        .fetch_one(&mut *connection).await?;
    let rows = sqlx::query("SELECT a.pid,a.backend_start,a.usename::text AS role,c.process_id,c.protocol,c.binary_version \
        FROM pg_catalog.pg_stat_activity a LEFT JOIN public.knowledge_clients c \
        ON c.backend_pid=a.pid AND c.backend_start=a.backend_start AND c.role_oid=a.usesysid \
        WHERE a.datid=(SELECT oid FROM pg_catalog.pg_database WHERE datname=current_database()) \
        AND a.backend_type='client backend' AND a.pid<>pg_backend_pid() ORDER BY a.pid")
        .fetch_all(&mut *connection).await?;
    let mut clients = Vec::new();
    for row in rows {
        let protocol: Option<i32> = row.try_get("protocol")?;
        clients.push(Client {
            backend_pid: row.try_get("pid")?,
            backend_start: row.try_get("backend_start")?,
            role: row.try_get("role")?,
            process_id: row.try_get("process_id")?,
            protocol,
            binary_version: row.try_get("binary_version")?,
            compatibility: match protocol {
                Some(protocol) if protocol >= marker.2 => Compatibility::CompatibleRegistration,
                Some(_) => Compatibility::ProtocolTooOld,
                None => Compatibility::Unregistered,
            },
        });
    }
    // Marker changes during an unconstrained observation invalidate this report.
    let after: (Uuid, i64, i32, String) = sqlx::query_as("SELECT database_id,generation,minimum_client,backend FROM public.knowledge_storage WHERE singleton")
        .fetch_one(connection).await?;
    ensure!(
        after == (marker.0, marker.1, marker.2, marker.3.clone()),
        "knowledge generation changed during client audit; retry under the migration lease"
    );
    Ok(Audit {
        database_id: marker.0,
        generation: marker.1,
        minimum_client: marker.2,
        backend: marker.3,
        sampled_at: marker.4,
        live_blockers: clients
            .iter()
            .filter(|c| c.compatibility != Compatibility::CompatibleRegistration)
            .count(),
        clients,
        remaining_verification: [
            "offline hosts and external editors",
            "session affinity of every endpoint",
            "recheck under the migration lease before transition",
        ],
    })
}
