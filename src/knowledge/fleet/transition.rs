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
