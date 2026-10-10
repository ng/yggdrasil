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

impl RollbackPlan {
    pub(in crate::knowledge::fleet) fn validate_cancellation(
        &self,
        receipt: &CancellationReceipt,
    ) -> Result<()> {
        ensure!(
            receipt.operation == self.operation()
                && receipt.request_sha256 == self.sha256
                && receipt.database_id == self.forward.database_id
                && receipt.source_generation == self.request.source_generation,
            "cancellation receipt differs from exact rollback request"
        );
        Ok(())
    }
    pub(in crate::knowledge::fleet) async fn cancellation_on(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        receipt: &CancellationReceipt,
    ) -> Result<()> {
        self.validate_cancellation(receipt)?;
        self.registered_identity_on(tx).await?;
        ensure!(
            self.host_phase_on(tx).await?.is_none(),
            "cancellation requires original active OKF generation"
        );
        let matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.knowledge_fleet_rollback_cancellations WHERE operation_id=$1 AND request_sha256=$2 AND database_id=$3 AND source_generation=$4)")
            .bind(receipt.operation).bind(&receipt.request_sha256).bind(receipt.database_id).bind(receipt.source_generation).fetch_one(&mut **tx).await?;
        ensure!(matches, "committed rollback cancellation required");
        Ok(())
    }
    pub async fn cancel_host(
        &self,
        forward: &ValidatedPlan,
        config: &crate::config::database::DeploymentConfig,
        participant: Uuid,
        receipt: &CancellationReceipt,
        known_fence: Option<&crate::knowledge::fence::LocalFence>,
        pool: &sqlx::PgPool,
    ) -> Result<crate::knowledge::fence::LocalCancellation> {
        use crate::{
            config::database::KnowledgeConfig,
            knowledge::fence::{CoordinatorBinding, FenceLease},
        };
        let expected = self.expected_binding(forward, participant)?;
        self.validate_cancellation(receipt)?;
        let host = forward
            .plan()
            .participants
            .iter()
            .find(|h| h.id == participant)
            .unwrap();
        ensure!(
            config.knowledge_dir == host.corpus
                && config.knowledge_policy_dir.canonicalize()? == host.policy,
            "cancellation differs from configured participant paths"
        );
        let config = KnowledgeConfig {
            data_dir: config.data_dir.clone(),
            knowledge_dir: config.knowledge_dir.clone(),
            knowledge_policy_dir: config.knowledge_policy_dir.clone(),
        };
        let local = FenceLease::acquire(&config)?;
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock_shared(1497843531,1)")
            .execute(&mut *tx)
            .await?;
        self.cancellation_on(&mut tx, receipt).await?;
        local.verify_shared(&forward.plan().shared)?;
        let result = local.cancel(
            self.request.source_generation,
            CoordinatorBinding {
                migration_operation: self.operation(),
                participant,
            },
            &expected,
            self.sha256(),
            known_fence,
        )?;
        local.verify_shared(&forward.plan().shared)?;
        tx.commit()
            .await
            .context("local cancellation published; retry exact request")?;
        Ok(result)
    }
}
