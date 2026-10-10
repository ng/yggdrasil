use super::*;

/// Committed barrier against delayed reverse-fence requests. This is not proof
/// that any host has restored its OKF selection or that a new request may start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationReceipt {
    pub operation: Uuid,
    pub request_sha256: String,
    pub database_id: Uuid,
    pub source_generation: i64,
}
impl RollbackPlan {
    pub(super) async fn not_cancelled_on(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        let cancelled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.knowledge_fleet_rollback_cancellations WHERE operation_id=$1)")
            .bind(self.operation()).fetch_one(&mut **tx).await?;
        ensure!(
            !cancelled,
            "rollback operation cancelled; complete host cancellation before replanning"
        );
        Ok(())
    }
    /// Drain in-flight host fences, then durably reject all delayed requests.
    /// SQL remains at the original OKF generation; existing local fences remain.
    pub async fn begin_cancellation(&self, pool: &sqlx::PgPool) -> Result<CancellationReceipt> {
        let mut tx = Registration::transaction(pool).await?;
        self.registered_identity_on(&mut tx).await?;
        ensure!(
            self.host_phase_on(&mut tx).await?.is_none(),
            "rollback cancellation is unavailable after the global SQL fence"
        );
        let receipt = CancellationReceipt {
            operation: self.operation(),
            request_sha256: self.sha256.clone(),
            database_id: self.forward.database_id,
            source_generation: self.request.source_generation,
        };
        let saved: Option<(String,Uuid,i64)> = sqlx::query_as("SELECT request_sha256,database_id,source_generation FROM public.knowledge_fleet_rollback_cancellations WHERE operation_id=$1")
            .bind(self.operation()).fetch_optional(&mut *tx).await?;
        if let Some(saved) = saved {
            ensure!(
                saved
                    == (
                        receipt.request_sha256.clone(),
                        receipt.database_id,
                        receipt.source_generation
                    ),
                "rollback cancellation differs from exact request"
            );
        } else {
            sqlx::query("INSERT INTO public.knowledge_fleet_rollback_cancellations(operation_id,request_sha256,database_id,source_generation) VALUES($1,$2,$3,$4)")
                .bind(receipt.operation).bind(&receipt.request_sha256).bind(receipt.database_id).bind(receipt.source_generation)
                .execute(&mut *tx).await?;
        }
        tx.commit()
            .await
            .context("rollback cancellation outcome uncertain; retry exact request")?;
        Ok(receipt)
    }
}
