//! Durable host-local OKF writer fencing. This does not transition PostgreSQL
//! or prove that other hosts/external editors have stopped writing.
use super::{
    document::digest,
    guard::CLIENT_PROTOCOL,
    runtime::{Binding, Phase, SELECTION_FILE},
    store::KnowledgeStore,
};
use crate::config::database::KnowledgeConfig;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{os::unix::fs::MetadataExt, path::PathBuf};
use uuid::Uuid;

mod sql;
pub use sql::{SqlPreparation, cancel_sql, prepare_sql, prepare_sql_at_source};

/// Request identity only; authenticated transport and a complete participant
/// census remain the coordinator's responsibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorBinding {
    pub migration_operation: Uuid,
    pub participant: Uuid,
}
impl CoordinatorBinding {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.migration_operation.is_nil() && !self.participant.is_nil(),
            "non-nil migration operation and participant IDs required"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32,
    operation: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coordinator: Option<CoordinatorBinding>,
    policy: PathBuf,
    policy_identity: (u64, u64),
    bundle_identity: (u64, u64),
    original: String,
    fenced: String,
}

#[derive(Debug, Serialize)]
pub struct LocalFence {
    pub operation: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<CoordinatorBinding>,
    pub database_id: Uuid,
    pub corpus_id: Uuid,
    /// Last selected OKF generation; the database has NOT advanced here.
    pub source_generation: i64,
    pub policy: PathBuf,
    pub original_sha256: String,
    pub fenced_sha256: String,
}

fn identity(path: &std::path::Path) -> Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir(), "selection directory changed");
    Ok((metadata.dev(), metadata.ino()))
}

impl Intent {
    fn validate(&self, config: &KnowledgeConfig, generation: i64) -> Result<Binding> {
        ensure!(
            self.version == if self.coordinator.is_some() { 2 } else { 1 }
                && !self.operation.is_nil(),
            "unsupported local fence journal"
        );
        if let Some(binding) = self.coordinator {
            binding.validate()?;
        }
        ensure!(
            config.knowledge_policy_dir.canonicalize()? == self.policy
                && identity(&self.policy)? == self.policy_identity,
            "local fence policy directory changed"
        );
        let mut binding: Binding = serde_json::from_str(&self.original)?;
        ensure!(
            binding.version == 1
                && binding.minimum_client > 0
                && binding.minimum_client <= CLIENT_PROTOCOL
                && binding.generation == generation
                && generation > 0
                && binding.phase == Phase::Okf,
            "local fence requires the expected compatible OKF generation"
        );
        ensure!(
            binding.bundle.is_absolute()
                && config.knowledge_dir.canonicalize()? == binding.bundle
                && identity(&binding.bundle)? == self.bundle_identity,
            "local fence bundle directory changed"
        );
        binding.phase = Phase::Fenced;
        ensure!(
            serde_json::to_string(&binding)? == self.fenced,
            "local fence journal does not preserve the original selection"
        );
        Ok(binding)
    }
}

/// Drain compatible local operations, persist retry input, then atomically fence
/// the saved selection. A retry accepts only the exact original or fenced bytes.
/// Generation remains the last selected OKF generation, not a claimed SQL fence.
/// There is deliberately no unfence operation: activation needs the full workflow.
pub fn local(config: &KnowledgeConfig, generation: i64) -> Result<LocalFence> {
    local_bound(config, generation, None)
}

/// Persist a local fence bound to the coordinator's operation and participant.
/// Retrying another request cannot adopt an existing fence or rewrite its owner.
/// This does not authenticate the participant or transition the database.
pub fn local_for_migration(
    config: &KnowledgeConfig,
    generation: i64,
    migration_operation: Uuid,
    participant: Uuid,
) -> Result<LocalFence> {
    local_bound(
        config,
        generation,
        Some(CoordinatorBinding {
            migration_operation,
            participant,
        }),
    )
}

fn local_bound(
    config: &KnowledgeConfig,
    generation: i64,
    coordinator: Option<CoordinatorBinding>,
) -> Result<LocalFence> {
    if let Some(binding) = coordinator {
        binding.validate()?;
    }
    ensure!(generation > 0, "positive expected generation required");
    let policy = KnowledgeStore::open(&config.knowledge_policy_dir, false)?;
    let path = config.knowledge_policy_dir.canonicalize()?;
    policy.verify_root_path(&path)?;
    let _selection = policy.selection_lease(true)?;
    let name = format!("local-fence-{generation}.json");
    let intent = if let Some(saved) = policy.read_artifact(&name)? {
        let intent: Intent = serde_json::from_str(&saved)?;
        intent.validate(config, generation)?;
        ensure!(
            intent.coordinator == coordinator,
            "local fence belongs to another migration operation or participant"
        );
        intent
    } else {
        let original = policy
            .read_control(SELECTION_FILE)?
            .context("no selected OKF corpus to fence")?;
        let mut binding: Binding = serde_json::from_str(&original)?;
        let bundle_identity = identity(&binding.bundle)?;
        binding.phase = Phase::Fenced;
        let intent = Intent {
            version: if coordinator.is_some() { 2 } else { 1 },
            operation: Uuid::new_v4(),
            coordinator,
            policy: path.clone(),
            policy_identity: identity(&path)?,
            bundle_identity,
            original,
            fenced: serde_json::to_string(&binding)?,
        };
        intent.validate(config, generation)?;
        // Saved and fsynced before selection publication. An interrupted command
        // reuses this operation instead of inferring success from current phase.
        policy.retain_artifact(&name, &serde_json::to_string(&intent)?, false)?;
        intent
    };
    let binding = intent.validate(config, generation)?;
    policy.verify_root_path(&path)?;
    let saved = serde_json::to_string(&intent)?;
    ensure!(
        policy.read_artifact(&name)?.as_deref() == Some(&saved),
        "local fence journal changed"
    );
    policy.update_control(SELECTION_FILE, |current| {
        ensure!(
            current == Some(intent.original.as_str()) || current == Some(intent.fenced.as_str()),
            "selection differs from retained local fence; preserve evidence and inspect"
        );
        Ok((intent.fenced.clone(), ()))
    })?;
    policy.verify_root_path(&path)?;
    intent.validate(config, generation)?;
    ensure!(
        policy.read_artifact(&name)?.as_deref() == Some(&saved),
        "local fence journal changed after publication; inspect retained evidence"
    );
    Ok(LocalFence {
        operation: intent.operation,
        coordinator: intent.coordinator,
        database_id: binding.mappings.database_id,
        corpus_id: binding.mappings.corpus_id,
        source_generation: generation,
        policy: path,
        original_sha256: digest(intent.original.as_bytes()),
        fenced_sha256: digest(intent.fenced.as_bytes()),
    })
}
