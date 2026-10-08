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
