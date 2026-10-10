//! Optional operational usage. Callers isolate telemetry errors from knowledge
//! writes/injection; no operation here reads or changes authoritative documents.
use super::legacy::Usage;
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Totals {
    pub corpus_id: Uuid,
    pub document_id: Uuid,
    pub applied_count: i64,
    pub last_applied_at: Option<DateTime<Utc>>,
    pub baseline_imported: bool,
}
impl Totals {
    /// Legacy models use i32. Never wrap or clamp totals outside that contract.
    pub fn legacy_usage(&self) -> Result<Usage> {
        Ok(Usage {
            corpus_id: self.corpus_id,
            document_id: self.document_id,
            applied_count: self.applied_count.try_into()?,
            last_applied_at: self.last_applied_at,
        })
    }
}

pub struct Telemetry<'a> {
    pool: &'a PgPool,
}
impl<'a> Telemetry<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Seed only the fenced legacy snapshot. A repeated identical seed is safe;
    /// a different baseline is a conflict, never an overwrite of observed usage.
    pub async fn seed(&self, usage: &Usage) -> Result<()> {
        let result = sqlx::query(
            r#"
            INSERT INTO knowledge_usage
                (corpus_id, document_id, imported_count, imported_last_applied_at)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (corpus_id, document_id) DO UPDATE SET
                imported_count = EXCLUDED.imported_count,
                imported_last_applied_at = EXCLUDED.imported_last_applied_at
            WHERE knowledge_usage.imported_count IS NULL
                OR (knowledge_usage.imported_count = EXCLUDED.imported_count
                    AND knowledge_usage.imported_last_applied_at IS NOT DISTINCT FROM
                        EXCLUDED.imported_last_applied_at)
        "#,
        )
        .bind(usage.corpus_id)
        .bind(usage.document_id)
        .bind(usage.applied_count)
        .bind(usage.last_applied_at)
        .execute(self.pool)
        .await?;
        ensure!(
            result.rows_affected() == 1,
            "conflicting imported telemetry baseline"
        );
        Ok(())
    }

    pub async fn get(&self, corpus: Uuid, document: Uuid) -> Result<Option<Totals>> {
        Ok(sqlx::query_as::<_, Totals>(
            r#"
            SELECT corpus_id, document_id,
                COALESCE(imported_count, 0)::bigint + observed_count AS applied_count,
                GREATEST(imported_last_applied_at, observed_last_applied_at) AS last_applied_at,
                imported_count IS NOT NULL AS baseline_imported
            FROM knowledge_usage WHERE corpus_id = $1 AND document_id = $2
        "#,
        )
        .bind(corpus)
        .bind(document)
        .fetch_optional(self.pool)
        .await?)
    }

    /// Use one stable application ID per actual injection, including retries.
    /// Returns true only for the first commit; duplicate IDs preserve the first
    /// timestamp. This does not authorize injection or implement session dedup.
    pub async fn record(
        &self,
        corpus: Uuid,
        document: Uuid,
        application: Uuid,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let inserted = record_on(&mut transaction, corpus, document, application, at).await?;
        transaction.commit().await?;
        Ok(inserted)
    }
}

/// One emitted rule, with a stable ID reused if its database transaction is retried.
/// These receipts are operational metadata; they never authorize a document.
#[derive(Clone)]
pub struct Application {
    pub document: Uuid,
    pub application: Uuid,
    pub at: DateTime<Utc>,
    pub imported: bool,
}

async fn record_on(
    connection: &mut PgConnection,
    corpus: Uuid,
    document: Uuid,
    application: Uuid,
    at: DateTime<Utc>,
) -> Result<bool> {
    // Match batch writers: acquire the usage row before touching its receipts.
    sqlx::query("INSERT INTO knowledge_usage (corpus_id, document_id) VALUES ($1, $2) ON CONFLICT (corpus_id, document_id) DO UPDATE SET observed_count = knowledge_usage.observed_count")
            .bind(corpus).bind(document).execute(&mut *connection).await?;
    let inserted = sqlx::query(
        r#"
            INSERT INTO knowledge_applications (corpus_id, document_id, application_id, applied_at)
            VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING
        "#,
    )
    .bind(corpus)
    .bind(document)
    .bind(application)
    .bind(at)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        sqlx::query(
            r#"
                UPDATE knowledge_usage SET observed_count = observed_count + 1,
                    observed_last_applied_at = GREATEST(observed_last_applied_at, $3)
                WHERE corpus_id = $1 AND document_id = $2
            "#,
        )
        .bind(corpus)
        .bind(document)
        .bind(at)
        .execute(&mut *connection)
        .await?;
    }
    Ok(inserted)
}

/// The caller holds the selected storage-generation transaction. All increments
/// commit together; returning totals does not mean the transaction committed.
pub async fn record_batch(
    connection: &mut PgConnection,
    corpus: Uuid,
    applications: &[Application],
) -> Result<Vec<Totals>> {
    use std::collections::BTreeMap;
    if applications.is_empty() {
        return Ok(Vec::new());
    }
    // Lock every existing AND missing usage row in the same order before any
    // receipt insert. Single-record writers take this lock too. Locking only
    // newly inserted rows first would deadlock mixed existing/missing batches.
    let mut ordered: Vec<_> = applications.iter().collect();
    ordered.sort_by_key(|a| (a.document, a.application));
    let mut documents: Vec<_> = ordered.iter().map(|a| a.document).collect();
    documents.dedup();
    let initial = sqlx::query_as::<_, Totals>(
        r#"INSERT INTO knowledge_usage (corpus_id, document_id)
           SELECT $1, document FROM unnest($2::uuid[]) AS document ORDER BY document
           ON CONFLICT (corpus_id, document_id) DO UPDATE
               SET observed_count = knowledge_usage.observed_count
           RETURNING corpus_id, document_id,
               COALESCE(imported_count, 0)::bigint + observed_count AS applied_count,
               GREATEST(imported_last_applied_at, observed_last_applied_at) AS last_applied_at,
               imported_count IS NOT NULL AS baseline_imported"#,
    )
    .bind(corpus)
    .bind(&documents)
    .fetch_all(&mut *connection)
    .await?;
    let mut current: BTreeMap<_, _> = initial.into_iter().map(|t| (t.document_id, t)).collect();

    // Stable sorting plus dedup retains the first timestamp for duplicate IDs,
    // including duplicates within this batch. PostgreSQL returns its stored
    // timestamp precision so intermediate totals match single-record writes.
    let mut unique = ordered.clone();
    unique.dedup_by_key(|a| (a.document, a.application));
    let documents: Vec<_> = unique.iter().map(|a| a.document).collect();
    let ids: Vec<_> = unique.iter().map(|a| a.application).collect();
    let times: Vec<_> = unique.iter().map(|a| a.at).collect();
    let inserted: Vec<(Uuid, Uuid, DateTime<Utc>, i64)> = sqlx::query_as(
        r#"WITH inserted AS (
               INSERT INTO knowledge_applications
                   (corpus_id, document_id, application_id, applied_at)
               SELECT $1, document, application, at
               FROM unnest($2::uuid[], $3::uuid[], $4::timestamptz[])
                   AS input(document, application, at)
               ORDER BY document, application
               ON CONFLICT DO NOTHING
               RETURNING document_id, application_id, applied_at
           ), delta AS (
               SELECT document_id, count(*) AS amount, max(applied_at) AS latest
               FROM inserted GROUP BY document_id
           ), updated AS (
               UPDATE knowledge_usage AS usage SET
                   observed_count = usage.observed_count + delta.amount,
                   observed_last_applied_at = GREATEST(usage.observed_last_applied_at, delta.latest)
               FROM delta WHERE usage.corpus_id = $1 AND usage.document_id = delta.document_id
               RETURNING usage.document_id,
                   COALESCE(usage.imported_count, 0)::bigint + usage.observed_count AS total
           )
           SELECT inserted.document_id, inserted.application_id, inserted.applied_at, updated.total
           FROM inserted JOIN updated USING (document_id)"#,
    )
    .bind(corpus)
    .bind(documents)
    .bind(ids)
    .bind(times)
    .fetch_all(&mut *connection)
    .await?;
    let mut inserted: BTreeMap<_, _> = inserted
        .into_iter()
        .map(|(doc, id, at, _)| ((doc, id), at))
        .collect();
    let mut totals = Vec::new();
    for application in ordered {
        let total = current
            .get_mut(&application.document)
            .expect("locked usage row");
        if let Some(at) = inserted.remove(&(application.document, application.application)) {
            total.applied_count = total
                .applied_count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("telemetry count overflow"))?;
            total.last_applied_at = Some(total.last_applied_at.map_or(at, |old| old.max(at)));
        }
        // A migration baseline cannot be inferred from post-cutover observations.
        if !application.imported || total.baseline_imported {
            totals.push(total.clone());
        }
    }
    Ok(totals)
}
