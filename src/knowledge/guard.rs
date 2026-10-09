//! Compatibility protocol for legacy knowledge operations. Hold the returned
//! transaction until the read/write finishes so a cutover drains in-flight work.
use sqlx::{PgPool, Postgres, Transaction};

pub const CLIENT_PROTOCOL: i32 = 1;

pub async fn legacy_transaction(
    pool: &PgPool,
    writing: bool,
    expected_generation: Option<i64>,
) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT public.ygg_knowledge_guard($1, $2, $3)")
        .bind(writing)
        .bind(CLIENT_PROTOCOL)
        .bind(expected_generation)
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

/// Lease the connected OKF generation. Acquire after the local selection lease;
/// cutover must use the same ordering and drain both before publication.
pub async fn selected_transaction(
    pool: &PgPool,
    database: uuid::Uuid,
    corpus: uuid::Uuid,
    generation: i64,
) -> anyhow::Result<Transaction<'static, Postgres>> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(1497843531, 1)")
        .execute(&mut *transaction)
        .await?;
    // The advisory lease stabilizes the marker. Avoid a row lock here: a
    // transitioning UPDATE may already hold the tuple lock while its trigger
    // waits for our shared advisory lease, which would invert lock ordering.
    let marker: (uuid::Uuid, i64, i32, String, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT database_id, generation, minimum_client, backend, corpus_id \
         FROM public.knowledge_storage WHERE singleton",
    )
    .fetch_one(&mut *transaction)
    .await?;
    anyhow::ensure!(
        marker.0 == database
            && marker.1 == generation
            && marker.2 > 0
            && marker.2 <= CLIENT_PROTOCOL
            && marker.3 == "okf"
            && marker.4 == Some(corpus),
        "connected knowledge generation differs from local selection"
    );
    Ok(transaction)
}
