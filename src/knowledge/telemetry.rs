//! Optional operational usage. Callers isolate telemetry errors from knowledge
//! writes/injection; no operation here reads or changes authoritative documents.
use super::legacy::Usage;
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

#[derive(Debug, FromRow, PartialEq)]
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
    sqlx::query("INSERT INTO knowledge_usage (corpus_id, document_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
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
    let mut totals = Vec::new();
    // Consistent row-lock ordering prevents overlapping batches from deadlocking
    // when two claims mention the same rules in different file orders.
    let mut ordered: Vec<_> = applications.iter().collect();
    ordered.sort_by_key(|a| (a.document, a.application));
    for application in ordered {
        record_on(
            connection,
            corpus,
            application.document,
            application.application,
            application.at,
        )
        .await?;
        let total = sqlx::query_as::<_, Totals>(
            "SELECT corpus_id, document_id, COALESCE(imported_count, 0)::bigint + observed_count AS applied_count, \
             GREATEST(imported_last_applied_at, observed_last_applied_at) AS last_applied_at, \
             imported_count IS NOT NULL AS baseline_imported FROM knowledge_usage WHERE corpus_id=$1 AND document_id=$2"
        ).bind(corpus).bind(application.document).fetch_one(&mut *connection).await?;
        // A migration baseline cannot be inferred from post-cutover observations.
        if !application.imported || total.baseline_imported {
            totals.push(total);
        }
    }
    Ok(totals)
}
