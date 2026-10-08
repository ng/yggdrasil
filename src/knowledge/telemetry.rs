//! Optional operational usage. Callers isolate telemetry errors from knowledge
//! writes/injection; no operation here reads or changes authoritative documents.
use super::legacy::Usage;
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
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
        sqlx::query("INSERT INTO knowledge_usage (corpus_id, document_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(corpus).bind(document).execute(&mut *transaction).await?;
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
        .execute(&mut *transaction)
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
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(inserted)
    }
}
