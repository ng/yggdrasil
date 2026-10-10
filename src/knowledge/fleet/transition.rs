//! Database half of fleet fencing/recovery. Only the coordinator journal supplies
//! freshly authenticated preparation evidence; these helpers are not public API.
use super::Registration;
use crate::knowledge::{clients, guard::CLIENT_PROTOCOL, source_backup::SourceBackup};
use anyhow::{Context, Result, ensure};
use sqlx::{PgPool, Postgres, Transaction};

type Evidence = (String, String);
async fn event(
    registration: &Registration,
    tx: &mut Transaction<'_, Postgres>,
    step: &str,
) -> Result<Option<Evidence>> {
    Ok(sqlx::query_as("SELECT prepared_sha256,backup_sha256 FROM public.knowledge_fleet_events WHERE operation_id=$1 AND step=$2")
        .bind(registration.operation).bind(step).fetch_optional(&mut **tx).await?)
}
async fn record(
    registration: &Registration,
    tx: &mut Transaction<'_, Postgres>,
    step: &str,
    evidence: &Evidence,
) -> Result<()> {
    sqlx::query("INSERT INTO public.knowledge_fleet_events(operation_id,step,prepared_sha256,backup_sha256) VALUES($1,$2,$3,$4)")
        .bind(registration.operation).bind(step).bind(&evidence.0).bind(&evidence.1)
        .execute(&mut **tx).await?;
    Ok(())
}
async fn registered(registration: &Registration, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    ensure!(
        registration.saved(tx).await? == Some(registration.expected()?),
        "fleet transition requires the exact registered plan"
    );
    let cancelled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.knowledge_migration_cancellations WHERE operation_id=$1)")
        .bind(registration.operation).fetch_one(&mut **tx).await?;
    ensure!(!cancelled, "fleet operation was cancelled before fencing");
    Ok(())
}
fn verify_fenced(registration: &Registration, marker: &super::Marker) -> Result<()> {
    ensure!(
        marker.0 == registration.database_id
            && marker.1 == registration.source_generation + 1
            && marker.2 > 0
            && marker.2 <= CLIENT_PROTOCOL
            && marker.3 == "fenced"
            && marker.4 == Some(registration.corpus_id),
        "registered fleet fence is no longer current"
    );
    Ok(())
}
/// Only chooses which host RPC to attempt. Every RPC and final transaction
/// independently rechecks current authority; this query does not grant authority.
pub(super) async fn has_fence(registration: &Registration, pool: &PgPool) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.knowledge_fleet_events WHERE operation_id=$1 AND step='fenced')")
        .bind(registration.operation).fetch_one(pool).await?)
}
pub(super) async fn fence(
    registration: &Registration,
    pool: &PgPool,
    backup: &SourceBackup,
    prepared_sha256: &str,
    verify_journal: impl Fn() -> Result<()>,
) -> Result<i64> {
    let evidence = (prepared_sha256.to_owned(), backup.digest().to_owned());
    let mut tx = Registration::transaction(pool).await?;
    registered(registration, &mut tx).await?;
    verify_journal()?;
    ensure!(
        event(registration, &mut tx, "aborted").await?.is_none(),
        "fleet fence was aborted"
    );
    let marker = Registration::marker(&mut tx).await?;
    if let Some(saved) = event(registration, &mut tx, "fenced").await? {
        ensure!(saved == evidence, "fleet fence evidence changed");
        verify_fenced(registration, &marker)?;
    } else {
        registration.verify_sql(&marker, registration.source_generation)?;
        ensure!(
            clients::audit(&mut tx).await?.live_blockers == 0,
            "unregistered or outdated clients block fleet fencing"
        );
        backup.verify_on(&mut tx).await?;
        verify_journal()?;
        sqlx::query("UPDATE public.knowledge_storage SET backend='fenced',generation=$1,corpus_id=$2 WHERE singleton")
            .bind(registration.source_generation+1).bind(registration.corpus_id).execute(&mut *tx).await?;
        record(registration, &mut tx, "fenced", &evidence).await?;
    }
    backup.verify_on(&mut tx).await?;
    verify_journal()?;
    tx.commit()
        .await
        .context("fleet fence outcome uncertain; resume the same journal")?;
    Ok(registration.source_generation + 1)
}
pub(super) async fn abort(
    registration: &Registration,
    pool: &PgPool,
    backup: &SourceBackup,
    prepared_sha256: &str,
    verify_journal: impl Fn() -> Result<()>,
    verify_publication: impl Fn() -> Result<()>,
) -> Result<i64> {
    let evidence = (prepared_sha256.to_owned(), backup.digest().to_owned());
    let mut tx = Registration::transaction(pool).await?;
    registered(registration, &mut tx).await?;
    verify_journal()?;
    ensure!(
        event(registration, &mut tx, "fenced").await? == Some(evidence.clone()),
        "matching fleet-owned fence required for abort"
    );
    let marker = Registration::marker(&mut tx).await?;
    if let Some(saved) = event(registration, &mut tx, "aborted").await? {
        ensure!(saved == evidence, "fleet abort evidence changed");
        registration.verify_sql(&marker, registration.source_generation + 2)?;
        // Do not compare or replay frozen source rows after SQL has reopened.
    } else {
        verify_fenced(registration, &marker)?;
        backup.verify_on(&mut tx).await?;
        verify_journal()?;
        verify_publication()?;
        sqlx::query("UPDATE public.knowledge_storage SET backend='sql',generation=$1,corpus_id=NULL WHERE singleton")
            .bind(registration.source_generation+2).execute(&mut *tx).await?;
        record(registration, &mut tx, "aborted", &evidence).await?;
    }
    tx.commit()
        .await
        .context("fleet abort outcome uncertain; resume abort with the same journal")?;
    Ok(registration.source_generation + 2)
}

/// Run a local phase only while this operation's exact fleet fence is current.
/// The callback must not acquire another database connection or contact hosts.
pub(super) async fn with_fenced_source<T>(
    registration: &Registration,
    pool: &PgPool,
    backup: &SourceBackup,
    prepared_sha256: &str,
    phase: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let mut tx = Registration::transaction(pool).await?;
    registered(registration, &mut tx).await?;
    let evidence = (prepared_sha256.to_owned(), backup.digest().to_owned());
    ensure!(
        event(registration, &mut tx, "fenced").await? == Some(evidence)
            && event(registration, &mut tx, "aborted").await?.is_none(),
        "local phase requires this operation's un-aborted fleet fence"
    );
    verify_fenced(registration, &Registration::marker(&mut tx).await?)?;
    backup.verify_on(&mut tx).await?;
    let result = phase()?;
    tx.commit()
        .await
        .context("fleet phase outcome uncertain; resume the same journal")?;
    Ok(result)
}

#[derive(Clone)]
pub(super) struct Activation {
    pub ready_sha256: String,
    pub ready_json: String,
    pub prepared_sha256: String,
    pub backup_sha256: String,
}
pub(super) async fn saved_activation(
    registration: &Registration,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<Option<Activation>> {
    let row: Option<(String,String,String,String)> = sqlx::query_as("SELECT ready_sha256,ready_json,prepared_sha256,backup_sha256 FROM public.knowledge_fleet_activations WHERE operation_id=$1")
        .bind(registration.operation).fetch_optional(&mut **tx).await?;
    Ok(row.map(
        |(ready_sha256, ready_json, prepared_sha256, backup_sha256)| Activation {
            ready_sha256,
            ready_json,
            prepared_sha256,
            backup_sha256,
        },
    ))
}
pub(super) async fn verify_activation(
    registration: &Registration,
    tx: &mut Transaction<'_, Postgres>,
    saved: &Activation,
) -> Result<()> {
    registered(registration, tx).await?;
    let marker = Registration::marker(tx).await?;
    ensure!(
        marker.0 == registration.database_id
            && marker.1 == registration.source_generation + 2
            && marker.2 > 0
            && marker.2 <= CLIENT_PROTOCOL
            && marker.3 == "okf"
            && marker.4 == Some(registration.corpus_id),
        "fleet activation is no longer current"
    );
    ensure!(
        crate::knowledge::document::digest(saved.ready_json.as_bytes()) == saved.ready_sha256
            && event(registration, tx, "fenced").await?
                == Some((saved.prepared_sha256.clone(), saved.backup_sha256.clone()))
            && event(registration, tx, "aborted").await?.is_none(),
        "fleet activation evidence differs"
    );
    let payload: serde_json::Value = serde_json::from_str(&saved.ready_json)?;
    let manifest_sha = payload["publication"]["manifest_sha256"]
        .as_str()
        .context("activation manifest missing")?;
    let forwarded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.knowledge_forward_receipts WHERE operation_id=$1 AND database_id=$2 AND corpus_id=$3 AND fenced_generation=$4 AND active_generation=$5 AND manifest_sha256=$6)")
        .bind(registration.operation).bind(registration.database_id).bind(registration.corpus_id)
        .bind(registration.source_generation+1).bind(registration.source_generation+2).bind(manifest_sha)
        .fetch_one(&mut **tx).await?;
    ensure!(
        forwarded,
        "fleet activation lacks matching forward/telemetry receipt"
    );
    Ok(())
}
pub(super) async fn activation(
    registration: &Registration,
    pool: &PgPool,
) -> Result<Option<Activation>> {
    let mut tx = Registration::transaction(pool).await?;
    let saved = saved_activation(registration, &mut tx).await?;
    if let Some(saved) = &saved {
        verify_activation(registration, &mut tx, saved).await?;
    }
    tx.commit().await?;
    Ok(saved)
}
pub(super) async fn activate(
    registration: &Registration,
    pool: &PgPool,
    backup: &SourceBackup,
    prepared_sha256: &str,
    ready_json: &str,
    manifest: &crate::knowledge::export::Manifest,
    verify_publication: impl Fn() -> Result<()>,
) -> Result<Activation> {
    let expected = Activation {
        ready_sha256: crate::knowledge::document::digest(ready_json.as_bytes()),
        ready_json: ready_json.to_owned(),
        prepared_sha256: prepared_sha256.to_owned(),
        backup_sha256: backup.digest().to_owned(),
    };
    let mut tx = Registration::transaction(pool).await?;
    registered(registration, &mut tx).await?;
    if let Some(saved) = saved_activation(registration, &mut tx).await? {
        verify_activation(registration, &mut tx, &saved).await?;
        ensure!(
            saved.ready_json == expected.ready_json
                && saved.prepared_sha256 == expected.prepared_sha256
                && saved.backup_sha256 == expected.backup_sha256,
            "concurrent activation differs"
        );
        tx.commit().await?;
        return Ok(saved);
    }
    verify_fenced(registration, &Registration::marker(&mut tx).await?)?;
    ensure!(
        event(registration, &mut tx, "fenced").await?
            == Some((
                expected.prepared_sha256.clone(),
                expected.backup_sha256.clone()
            ))
            && event(registration, &mut tx, "aborted").await?.is_none(),
        "activation requires matching fleet fence"
    );
    backup.verify_on(&mut tx).await?;
    verify_publication()?;
    ensure!(
        crate::knowledge::forward::activate_on(&mut tx, registration.operation, manifest).await?
            == crate::knowledge::forward::Outcome::Activated,
        "fleet activation cannot adopt a prior private activation"
    );
    verify_publication()?;
    sqlx::query("INSERT INTO public.knowledge_fleet_activations(operation_id,generation,prepared_sha256,backup_sha256,ready_sha256,ready_json) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(registration.operation).bind(registration.source_generation+2).bind(&expected.prepared_sha256).bind(&expected.backup_sha256)
        .bind(&expected.ready_sha256).bind(&expected.ready_json).execute(&mut *tx).await?;
    tx.commit()
        .await
        .context("fleet activation outcome uncertain; resume exact journal")?;
    Ok(expected)
}
pub(super) async fn with_activation<T>(
    registration: &Registration,
    pool: &PgPool,
    expected: &str,
    phase: impl FnOnce(&Activation) -> Result<T>,
) -> Result<T> {
    let mut tx = Registration::transaction(pool).await?;
    let saved = saved_activation(registration, &mut tx)
        .await?
        .context("fleet activation missing")?;
    verify_activation(registration, &mut tx, &saved).await?;
    ensure!(saved.ready_sha256 == expected, "fleet activation changed");
    let result = phase(&saved)?;
    tx.commit().await?;
    Ok(result)
}
