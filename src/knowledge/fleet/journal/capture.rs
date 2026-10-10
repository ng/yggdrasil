//! Retain CURRENT shared data and policy evidence before any reverse SQL import.
use super::*;
use crate::{
    config::database::{DeploymentConfig, KnowledgeConfig},
    knowledge::{
        document::digest,
        fence::{CoordinatorBinding, FenceLease, LocalFence},
        fleet::rollback::RollbackPlan,
        reverse::{self, Candidate, RecoveryEvidence},
        runtime::SELECTION_FILE,
        shared::SharedGit,
    },
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackCapture {
    version: u32,
    pub operation: uuid::Uuid,
    pub fenced_generation: i64,
    pub shared_commit: String,
    pub intent_sha256: String,
    pub candidate: Candidate,
    pub evidence: RecoveryEvidence,
}
impl Journal {
    /// An unselected coordinator policy is allowed. If this coordinator is also
    /// a participant, its actual policy must retain the exact committed fence.
    fn capture_local_policy(
        &self,
        reverse: &RollbackPlan,
        config: &DeploymentConfig,
        policy: &KnowledgeStore,
        lease: &FenceLease<'_>,
        generation: i64,
        hosts: &str,
    ) -> Result<Option<LocalFence>> {
        if policy.read_control(SELECTION_FILE)?.is_none() {
            return Ok(None);
        }
        let intent: serde_json::Value = serde_json::from_str(
            &policy
                .read_artifact(&format!("local-fence-{}.json", generation - 1))?
                .context("coordinator policy lacks rollback fence")?,
        )?;
        let coordinator: CoordinatorBinding =
            serde_json::from_value(intent["coordinator"].clone())?;
        ensure!(
            coordinator.migration_operation == reverse.operation(),
            "coordinator policy belongs to another rollback"
        );
        let host = self
            .plan
            .plan()
            .participants
            .iter()
            .find(|h| h.id == coordinator.participant)
            .context("coordinator not in fleet census")?;
        ensure!(
            config.knowledge_dir == host.corpus
                && config.knowledge_policy_dir.canonicalize()? == host.policy,
            "coordinator policy paths differ from participant"
        );
        lease.verify_shared(&self.plan.plan().shared)?;
        let fence = lease.inspect(
            generation - 1,
            coordinator,
            &reverse.expected_binding(&self.plan, host.id)?,
        )?;
        let evidence: serde_json::Value = serde_json::from_str(hosts)?;
        let value = serde_json::to_value(&fence)?;
        ensure!(
            evidence["participants"]
                .as_array()
                .context("fence census missing")?
                .contains(&value),
            "coordinator local fence differs from committed census"
        );
        Ok(Some(fence))
    }
    /// Capture a lossless reverse candidate from the confirmed current Git tree
    /// and current SQL telemetry. Original legacy rows are only checked for drift;
    /// they are never substituted for post-cutover data. This leaves SQL fenced.
    pub async fn capture_rollback(
        &self,
        reverse: &RollbackPlan,
        config: &DeploymentConfig,
        pool: &sqlx::PgPool,
        ssh_identity: Option<&Path>,
    ) -> Result<RollbackCapture> {
        let fence = self
            .fence_rollback_hosts(reverse, pool, ssh_identity)
            .await?;
        let backup = self.source_backup(config)?;
        let original = self.verified_export()?;
        let original_sha = digest(&serde_json::to_vec(&original)?);
        let prefix = format!("rollback-{}", reverse.operation());
        let hosts = self
            .store
            .read_artifact(&format!("{prefix}-hosts.json"))?
            .context("reverse fence seal missing")?;
        ensure!(
            digest(hosts.as_bytes()) == fence.hosts_sha256,
            "reverse census changed before capture"
        );
        let cache = self.intent.directory.join(format!("{prefix}-cache"));
        ensure!(cache.is_dir(), "committed rollback cache missing");
        let git = SharedGit::open(&cache, self.plan.plan().shared.clone())?;
        git.verify_current_snapshot(&fence.remote_commit)?;
        let knowledge_config = KnowledgeConfig {
            data_dir: config.data_dir.clone(),
            knowledge_dir: config.knowledge_dir.clone(),
            knowledge_policy_dir: config.knowledge_policy_dir.clone(),
        };
        let lease = FenceLease::acquire(&knowledge_config)?;
        let policy_path = config.knowledge_policy_dir.canonicalize()?;
        let policy = KnowledgeStore::open(&policy_path, false)?;
        let local_fence =
            self.capture_local_policy(reverse, config, &policy, &lease, fence.generation, &hosts)?;
        let intent = serde_json::to_string(
            &serde_json::json!({"version":1,"rollback_sha256":reverse.sha256(),
            "fence":fence,"source_backup_sha256":backup.digest(),"original_sha256":original_sha,
            "cache":cache,"cache_identity":super::identity(&cache)?,
            "policy":policy_path,"policy_identity":super::identity(&policy_path)?,"local_fence":local_fence}),
        )?;
        let intent_name = format!("{prefix}-capture-intent.json");
        self.store.retain_artifact(&intent_name, &intent, false)?;
        let corpus = KnowledgeStore::open(&cache, false)?;
        let recovery = corpus.complete_pair_retained(
            &policy,
            &self
                .intent
                .directory
                .join(format!("{prefix}-corpus-backup")),
            &self
                .intent
                .directory
                .join(format!("{prefix}-policy-backup")),
        )?;
        let mut tx = reverse
            .fenced_transaction(pool, &fence.hosts_sha256)
            .await?;
        backup.verify_on(&mut tx).await?;
        let captured = reverse::capture_shared_recovery_on(
            &mut tx,
            &original,
            &git,
            &recovery,
            fence.generation,
        )
        .await?;
        ensure!(
            captured.commit == fence.remote_commit,
            "current rollback commit changed"
        );
        let evidence = RecoveryEvidence::new(
            &captured.candidate,
            &recovery,
            Some(captured.commit.clone()),
        )?;
        let result = RollbackCapture {
            version: 1,
            operation: reverse.operation(),
            fenced_generation: fence.generation,
            shared_commit: captured.commit,
            intent_sha256: digest(intent.as_bytes()),
            candidate: captured.candidate,
            evidence,
        };
        self.verify()?;
        ensure!(
            self.store.read_artifact(&intent_name)?.as_deref() == Some(intent.as_str())
                && self
                    .store
                    .read_artifact(&format!("{prefix}-hosts.json"))?
                    .as_deref()
                    == Some(hosts.as_str()),
            "rollback capture evidence changed"
        );
        ensure!(
            self.capture_local_policy(reverse, config, &policy, &lease, fence.generation, &hosts)?
                == local_fence,
            "coordinator policy fence changed during capture"
        );
        recovery.verify_paths(&cache, &policy_path)?;
        git.verify_recovery(&recovery, &result.shared_commit)?;
        self.store.retain_artifact(
            &format!("{prefix}-capture.json"),
            &serde_json::to_string(&result)?,
            false,
        )?;
        git.verify_recovery(&recovery, &result.shared_commit)?;
        tx.rollback().await?;
        Ok(result)
    }
}
