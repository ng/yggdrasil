//! Transaction-bound ownership evidence for recovery phase transitions.
use anyhow::{Result, ensure};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub(crate) async fn owner_transaction(pool: &PgPool) -> Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await?;
    let owner:bool=sqlx::query_scalar("SELECT pg_catalog.pg_has_role(session_user,relowner,'USAGE') AND pg_catalog.pg_has_role(current_user,relowner,'USAGE') FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass").fetch_one(&mut *tx).await?;
    ensure!(owner, "knowledge recovery requires migration owner");
    sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
pub(crate) struct Event {
    pub operation: Uuid,
    pub step: &'static str,
    pub request: String,
    pub database: Uuid,
    pub corpus: Uuid,
    pub generation: i64,
}
impl Event {
    pub async fn recorded(&self, tx: &mut Transaction<'_, Postgres>) -> Result<bool> {
        let row:Option<(String,Uuid,Uuid,i64)>=sqlx::query_as("SELECT request_sha256,database_id,corpus_id,generation FROM public.knowledge_recovery_events WHERE operation_id=$1 AND step=$2")
            .bind(self.operation).bind(self.step).fetch_optional(&mut **tx).await?;
        if let Some(row) = row {
            ensure!(
                row == (
                    self.request.clone(),
                    self.database,
                    self.corpus,
                    self.generation
                ),
                "recovery event conflicts with saved operation"
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }
    pub async fn record(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        sqlx::query("INSERT INTO public.knowledge_recovery_events(operation_id,step,request_sha256,database_id,corpus_id,generation) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(self.operation).bind(self.step).bind(&self.request).bind(self.database).bind(self.corpus).bind(self.generation).execute(&mut **tx).await?;
        Ok(())
    }
}
