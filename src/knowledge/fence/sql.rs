//! Participant preparation primitive for the future fleet coordinator. There is
//! deliberately no CLI entry point until authenticated activation/abort exists.
use super::{CoordinatorBinding, identity};
use crate::{
    config::database::KnowledgeConfig,
    knowledge::{
        document::digest,
        guard::CLIENT_PROTOCOL,
        runtime::{Binding, Phase, SELECTION_FILE},
        store::KnowledgeStore,
    },
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32,
    coordinator: CoordinatorBinding,
    policy: PathBuf,
    policy_identity: (u64, u64),
    bundle_identity: (u64, u64),
    identity_policy: Option<String>,
    shared_policy: Option<String>,
    /// Original selection is absent; never invent an original OKF generation.
    fenced: String,
}

#[derive(Debug, Serialize)]
pub struct SqlPreparation {
    pub coordinator: CoordinatorBinding,
    pub source_generation: i64,
    pub database_id: uuid::Uuid,
    pub corpus_id: uuid::Uuid,
    pub policy: PathBuf,
    pub intent_sha256: String,
    pub fenced_sha256: String,
}

/// Fence subsequent local commands on an unselected SQL host. The caller must
/// first back up host-local configuration/policy and authenticate this request.
/// This does not drain existing SQL transactions, change database authority, or
/// authorize activation. The coordinator must retain/validate the full receipt.
/// Both local roots must already exist; preparation never creates a corpus.
/// No local unfence operation is provided without a verified coordinator event.
pub fn prepare_sql(
    config: &KnowledgeConfig,
    binding: &Binding,
    coordinator: CoordinatorBinding,
) -> Result<SqlPreparation> {
    coordinator.validate()?;
    ensure!(
        binding.version == 1
            && binding.minimum_client == CLIENT_PROTOCOL
            && binding.generation > 0
            && binding.generation < i64::MAX - 2
            && binding.phase == Phase::Fenced
            && !binding.mappings.database_id.is_nil()
            && !binding.mappings.corpus_id.is_nil(),
        "SQL preparation requires an explicit compatible fenced source binding"
    );
    let policy_path = config.knowledge_policy_dir.canonicalize()?;
    let bundle_path = config.knowledge_dir.canonicalize()?;
    ensure!(
        binding.bundle.is_absolute() && binding.bundle == bundle_path,
        "SQL preparation bundle differs from local configuration"
    );
    ensure!(
        !bundle_path.starts_with(&policy_path) && !policy_path.starts_with(&bundle_path),
        "SQL preparation corpus and policy must be separate"
    );
    let policy = KnowledgeStore::open(&policy_path, false)?;
    let bundle = KnowledgeStore::open(&bundle_path, false)?;
    let _lease = policy.selection_lease(true)?;
    policy.verify_root_path(&policy_path)?;
    bundle.verify_root_path(&bundle_path)?;
    let desired = Intent {
        version: 1,
        coordinator,
        policy: policy_path.clone(),
        policy_identity: identity(&policy_path)?,
        bundle_identity: identity(&bundle_path)?,
        identity_policy: policy.read_control("identity.json")?,
        shared_policy: policy.read_control("shared.json")?,
        fenced: serde_json::to_string(binding)?,
    };
    let bytes = serde_json::to_string(&desired)?;
    let name = format!("sql-fence-{}.json", binding.generation);
    if let Some(saved) = policy.read_artifact(&name)? {
        let _: Intent = serde_json::from_str(&saved)?;
        ensure!(
            saved == bytes,
            "SQL preparation request, directories or policy changed"
        );
    } else {
        ensure!(
            policy.read_control(SELECTION_FILE)?.is_none(),
            "SQL preparation requires an absent local selection"
        );
        // Retain exact original absence and request before publishing the fence.
        policy.retain_artifact(&name, &bytes, false)?;
    }
    let verify = || -> Result<()> {
        policy.verify_root_path(&policy_path)?;
        bundle.verify_root_path(&bundle_path)?;
        ensure!(
            config.knowledge_policy_dir.canonicalize()? == policy_path
                && config.knowledge_dir.canonicalize()? == bundle_path
                && identity(&policy_path)? == desired.policy_identity
                && identity(&bundle_path)? == desired.bundle_identity
                && policy.read_control("identity.json")? == desired.identity_policy
                && policy.read_control("shared.json")? == desired.shared_policy
                && policy.read_artifact(&name)?.as_deref() == Some(bytes.as_str()),
            "SQL preparation configuration, policy or journal changed"
        );
        Ok(())
    };
    verify()?;
    policy.update_control(SELECTION_FILE, |current| {
        ensure!(
            current.is_none() || current == Some(desired.fenced.as_str()),
            "SQL preparation cannot replace an independent selection"
        );
        Ok((desired.fenced.clone(), ()))
    })?;
    verify()?;
    Ok(SqlPreparation {
        coordinator,
        source_generation: binding.generation,
        database_id: binding.mappings.database_id,
        corpus_id: binding.mappings.corpus_id,
        policy: policy_path,
        intent_sha256: digest(bytes.as_bytes()),
        fenced_sha256: digest(desired.fenced.as_bytes()),
    })
}
