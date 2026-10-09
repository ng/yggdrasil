//! Database half of forward activation, inside the coordinator's transaction.
//! A receipt proves this SQL step only: the coordinator must durably save its
//! operation ID, verify publication/backups, quiesce every participating host
//! and external editor, and retain local selection/corpus leases through commit.
//! No local binding is published here, and a live-client audit is not a fleet
//! census or an admission barrier for clients arriving after the sample.
use super::{
    document::digest, export::Manifest, guard::CLIENT_PROTOCOL, inventory, legacy::Mappings,
};
use anyhow::{Result, anyhow, ensure};
use sqlx::{Acquire, PgConnection, Postgres, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Activated,
    PreviouslyActivated,
}

/// The result is provisional until the caller commits. Retry an uncertain
/// commit using the same saved operation ID and manifest. A later generation,
/// changed frozen rows or changed imported baseline requires explicit recovery.
/// Uses one connection, including size-one pools. On error its savepoint undoes
/// all effects even if the caller subsequently commits unrelated outer work.
pub async fn activate_on(
    transaction: &mut Transaction<'_, Postgres>,
    operation: Uuid,
    manifest: &Manifest,
) -> Result<Outcome> {
    let mut savepoint = transaction.begin().await?;
    match activate_inner(&mut savepoint, operation, manifest).await {
        Ok(outcome) => {
            savepoint.commit().await?;
            Ok(outcome)
        }
        Err(error) => {
            savepoint.rollback().await?;
            Err(error)
        }
    }
}

async fn activate_inner(
    connection: &mut PgConnection,
    operation: Uuid,
    manifest: &Manifest,
) -> Result<Outcome> {
    ensure!(
        !operation.is_nil()
            && manifest.version == 1
            && manifest.generation > 0
            && !manifest.database_id.is_nil()
            && !manifest.corpus_id.is_nil(),
        "invalid forward activation identity/version"
    );
    let active = manifest
        .generation
        .checked_add(1)
        .ok_or_else(|| anyhow!("generation overflow"))?;
    let bytes = serde_json::to_vec(manifest)?;
    ensure!(
        bytes.len() <= 64 * 1024 * 1024 && manifest.entries.len() <= 100_000,
        "forward activation manifest exceeds limits"
    );
    let hash = digest(&bytes);
    let mappings: Mappings = serde_json::from_value(manifest.mappings.clone())?;
    ensure!(
        mappings.database_id == manifest.database_id && mappings.corpus_id == manifest.corpus_id,
        "manifest mapping identity differs"
    );
    let expected: BTreeMap<_, _> = manifest.entries.iter().map(|e| (e.key.id, e)).collect();
    ensure!(
        expected.len() == manifest.entries.len(),
        "duplicate manifest UUID"
    );
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut *connection)
        .await?;
    ensure!(
        isolation == "read committed",
        "forward activation requires READ COMMITTED"
    );
    let owner: bool = sqlx::query_scalar("SELECT pg_catalog.pg_has_role(session_user,relowner,'USAGE') AND pg_catalog.pg_has_role(current_user,relowner,'USAGE') FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass")
        .fetch_one(&mut *connection).await?;
    ensure!(owner, "forward activation requires migration owner");
    sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)")
        .execute(&mut *connection)
        .await?;
    let marker: (Uuid, i64, i32, String, Option<Uuid>) = sqlx::query_as("SELECT database_id,generation,minimum_client,backend,corpus_id FROM public.knowledge_storage WHERE singleton FOR UPDATE")
        .fetch_one(&mut *connection).await?;
    ensure!(
        marker.0 == manifest.database_id
            && marker.4 == Some(manifest.corpus_id)
            && marker.2 > 0
            && marker.2 <= CLIENT_PROTOCOL,
        "incompatible forward activation marker"
    );
    let prior: Option<(Uuid, Uuid, i64, i64, String)> = sqlx::query_as("SELECT database_id,corpus_id,fenced_generation,active_generation,manifest_sha256 FROM public.knowledge_forward_receipts WHERE operation_id=$1")
        .bind(operation).fetch_optional(&mut *connection).await?;
    let retry = if let Some(prior) = prior {
        ensure!(
            prior
                == (
                    manifest.database_id,
                    manifest.corpus_id,
                    manifest.generation,
                    active,
                    hash.clone()
                ),
            "operation ID belongs to a different forward activation"
        );
        ensure!(
            marker.1 == active && marker.3 == "okf",
            "recorded activation is no longer the selected generation"
        );
        true
    } else {
        ensure!(
            marker.1 == manifest.generation && marker.3 == "fenced",
            "forward activation requires expected fenced generation"
        );
        let audit = super::clients::audit(connection).await?;
        ensure!(
            audit.live_blockers == 0,
            "unregistered or outdated live clients block forward activation"
        );
        false
    };
    // An unaware writer may already hold RowExclusive while waiting on our
    // advisory fence. ACCESS SHARE freezes DDL without inverting that ordering;
    // the fenced generation and exclusive migration lease freeze row writers.
    sqlx::query("LOCK TABLE public.memories, public.learnings IN ACCESS SHARE MODE")
        .execute(&mut *connection)
        .await?;
    for (table, fields) in [
        (
            "public.memories",
            "created_at,created_by,memory_id,repo_id,text,user_id",
        ),
        (
            "public.learnings",
            "applied_count,approved_at,approved_by,context,created_at,created_by,file_glob,last_applied_at,learning_id,repo_id,rule_id,scope_tags,source,status,text,user_id",
        ),
    ] {
        let actual: String = sqlx::query_scalar("SELECT string_agg(attname::text,',' ORDER BY attname::text COLLATE \"C\") FROM pg_catalog.pg_attribute WHERE attrelid=$1::regclass AND attnum>0 AND NOT attisdropped")
            .bind(table).fetch_one(&mut *connection).await?;
        ensure!(
            actual == fields,
            "unsupported {table} schema for forward activation"
        );
    }
    sqlx::query("SET LOCAL TIME ZONE 'UTC'")
        .execute(&mut *connection)
        .await?;
    let mut observed = 0;
    let report = inventory::visit_on(connection, Some(&mappings), |_, row, doc, usage| {
        let key = super::store::Key::from_document(doc)?;
        let entry = expected
            .get(&key.id)
            .ok_or_else(|| anyhow!("source row missing from activation manifest"))?;
        ensure!(
            entry.key == key
                && entry.source_digest == row.source_digest
                && Some(&entry.document_digest) == row.document_digest.as_ref()
                && entry.usage.as_ref() == usage,
            "source row differs from activation manifest"
        );
        observed += 1;
        Ok(())
    })
    .await?;
    ensure!(
        report.rows_verified && observed == expected.len(),
        "forward activation requires complete verified source inventory"
    );

    // Include absent rows in the telemetry fence. Observations have no lease
    // dependency on this table lock; imported baselines must not change while
    // recovery validates them. Never reset observed counters/application IDs.
    sqlx::query("LOCK TABLE public.knowledge_usage IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *connection)
        .await?;
    if retry {
        let matching: i64 = sqlx::query_scalar("SELECT count(*) FROM public.learnings l JOIN public.knowledge_usage u ON u.document_id=l.learning_id AND u.corpus_id=$1 WHERE u.imported_count=l.applied_count AND u.imported_last_applied_at IS NOT DISTINCT FROM l.last_applied_at")
            .bind(manifest.corpus_id).fetch_one(&mut *connection).await?;
        ensure!(
            matching as usize == report.learnings,
            "imported telemetry changed after recorded activation"
        );
        return Ok(Outcome::PreviouslyActivated);
    }
    let seeded = sqlx::query("INSERT INTO public.knowledge_usage(corpus_id,document_id,imported_count,imported_last_applied_at) SELECT $1,learning_id,applied_count,last_applied_at FROM public.learnings ORDER BY learning_id ON CONFLICT(corpus_id,document_id) DO UPDATE SET imported_count=EXCLUDED.imported_count,imported_last_applied_at=EXCLUDED.imported_last_applied_at WHERE knowledge_usage.imported_count IS NULL OR (knowledge_usage.imported_count=EXCLUDED.imported_count AND knowledge_usage.imported_last_applied_at IS NOT DISTINCT FROM EXCLUDED.imported_last_applied_at)")
        .bind(manifest.corpus_id).execute(&mut *connection).await?;
    ensure!(
        seeded.rows_affected() as usize == report.learnings,
        "conflicting imported telemetry baseline"
    );
    sqlx::query("UPDATE public.knowledge_storage SET backend='okf',generation=$1 WHERE singleton")
        .bind(active)
        .execute(&mut *connection)
        .await?;
    sqlx::query("INSERT INTO public.knowledge_forward_receipts(operation_id,database_id,corpus_id,fenced_generation,active_generation,manifest_sha256) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(operation).bind(manifest.database_id).bind(manifest.corpus_id)
        .bind(manifest.generation).bind(active).bind(hash).execute(connection).await?;
    Ok(Outcome::Activated)
}
