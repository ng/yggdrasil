use super::*;
use crate::{
    config::database::{DeploymentConfig, KnowledgeConfig},
    knowledge::{
        fence::{CoordinatorBinding, FenceLease, LocalFence},
        fleet::journal::SqlReturnReceipt,
    },
};
impl RollbackPlan {
    pub(in crate::knowledge::fleet) fn validate_return(
        &self,
        forward: &ValidatedPlan,
        receipt: &SqlReturnReceipt,
    ) -> Result<crate::knowledge::recovery_event::Event> {
        ensure!(hex(&receipt.capture_sha256, 64), "invalid capture digest");
        ensure!(
            forward.registration().request_sha256 == self.forward.request_sha256,
            "SQL return forward request differs"
        );
        let event = self.return_event(
            &receipt.capture_sha256,
            &forward.plan().source_backup.manifest_sha256,
        );
        ensure!(
            receipt.operation == self.operation()
                && receipt.generation == event.generation
                && receipt.request_sha256 == event.request,
            "SQL return receipt differs from rollback request"
        );
        Ok(event)
    }
    pub async fn deselect_host(
        &self,
        forward: &ValidatedPlan,
        config: &DeploymentConfig,
        participant: Uuid,
        receipt: &SqlReturnReceipt,
        pool: &sqlx::PgPool,
    ) -> Result<LocalFence> {
        let expected = self.expected_binding(forward, participant)?;
        let event = self.validate_return(forward, receipt)?;
        let host = forward
            .plan()
            .participants
            .iter()
            .find(|h| h.id == participant)
            .unwrap();
        ensure!(
            config.knowledge_dir == host.corpus
                && config.knowledge_policy_dir.canonicalize()? == host.policy,
            "SQL return differs from configured participant paths"
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
        ensure!(
            self.returned_on(&mut tx, &event).await?,
            "committed SQL return required before deselection"
        );
        let saved = self
            .saved_fence_on(&mut tx)
            .await?
            .context("committed reverse fence missing")?;
        ensure!(
            digest(saved.hosts_json.as_bytes()) == saved.hosts_sha256,
            "committed host census changed"
        );
        let hosts: serde_json::Value = serde_json::from_str(&saved.hosts_json)?;
        let host = hosts["participants"]
            .as_array()
            .context("committed census missing")?
            .iter()
            .find(|h| h["coordinator"]["participant"] == serde_json::json!(participant))
            .context("participant missing from committed reverse fence")?;
        let committed: LocalFence = serde_json::from_value(host.clone())?;
        local.verify_shared(&forward.plan().shared)?;
        let result = local.deselect(
            self.request.source_generation,
            CoordinatorBinding {
                migration_operation: self.operation(),
                participant,
            },
            &expected,
            &committed,
            &event.request,
        )?;
        local.verify_shared(&forward.plan().shared)?;
        tx.commit()
            .await
            .context("local deselection published; retry exact SQL return")?;
        Ok(result)
    }
}
