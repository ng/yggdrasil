//! Offline command context selected by a durable local cutover binding. Absence
//! preserves legacy SQL behavior; malformed/fenced selections never fall back.
use super::{
    identity::{GitIdentity, IdentityRegistry},
    legacy::Mappings,
    service::KnowledgeService,
    store::KnowledgeStore,
};
use crate::config::database::{Environment, KnowledgeConfig};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const SELECTION_FILE: &str = "runtime.json";
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Fenced,
    Okf,
}

/// Published only by explicit cutover, after corpus validation and SQL fencing.
/// This is local authority during outages, not a remotely refreshed fleet lease.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub version: u32,
    pub minimum_client: i32,
    pub generation: i64,
    pub phase: Phase,
    pub bundle: PathBuf,
    pub mappings: Mappings,
    pub agents: BTreeMap<String, Uuid>,
}

pub struct Context {
    pub service: KnowledgeService,
    pub default_agent_name: String,
    pub mappings: Mappings,
    registry: IdentityRegistry,
    agents: BTreeMap<String, Uuid>,
    _selection_lease: File,
}
impl Context {
    pub fn from_environment(env: Environment) -> Result<Option<Self>> {
        let (config, env) = KnowledgeConfig::load(env)?;
        let user = env
            .get("YGG_USER")
            .cloned()
            .unwrap_or_else(crate::db::resolve_user);
        let mut context = Self::open(&config, &user)?;
        if let Some(context) = &mut context {
            context.default_agent_name = env.get("YGG_AGENT_NAME").cloned().unwrap_or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "ygg".into())
            });
        }
        Ok(context)
    }
    pub fn open(config: &KnowledgeConfig, legacy_user: &str) -> Result<Option<Self>> {
        match std::fs::symlink_metadata(&config.knowledge_policy_dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let policy = KnowledgeStore::open(&config.knowledge_policy_dir, false)?;
        // Do not create even a lease for an unselected legacy installation.
        if policy.read_control(SELECTION_FILE)?.is_none() {
            return Ok(None);
        }
        let lease = policy.selection_lease(false)?;
        let binding: Binding = serde_json::from_str(
            &policy
                .read_control(SELECTION_FILE)?
                .ok_or_else(|| anyhow!("knowledge selection changed; retry command"))?,
        )?;
        ensure!(
            binding.version == 1
                && binding.minimum_client > 0
                && binding.minimum_client <= super::guard::CLIENT_PROTOCOL
                && binding.generation > 0,
            "unsupported knowledge storage generation/client"
        );
        ensure!(
            binding.phase == Phase::Okf,
            "knowledge cutover is fenced; resume or abort migration"
        );
        ensure!(
            binding.bundle.is_absolute()
                && binding.bundle == config.knowledge_dir.canonicalize()?,
            "selected knowledge bundle differs from configured directory"
        );
        let registry = IdentityRegistry::open(&config.knowledge_policy_dir, false)?;
        let identities = registry.read()?.0;
        ensure!(
            identities.corpus_id == binding.mappings.corpus_id,
            "knowledge corpus identity mismatch"
        );
        for (legacy, portable) in &binding.mappings.repos {
            ensure!(
                registry.from_legacy(binding.mappings.database_id, *legacy)? == *portable,
                "knowledge repository mapping differs from policy"
            );
        }
        let user = binding
            .mappings
            .users
            .get(legacy_user)
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| anyhow!("explicit knowledge user mapping required"))?
            .clone();
        let service = KnowledgeService::new(
            KnowledgeStore::open(&config.knowledge_dir, false)?,
            IdentityRegistry::open(&config.knowledge_policy_dir, false)?,
            user,
        )?;
        Ok(Some(Self {
            service,
            default_agent_name: "ygg".into(),
            mappings: binding.mappings,
            registry,
            agents: binding.agents,
            _selection_lease: lease,
        }))
    }
    pub fn repo(&self, cwd: &Path) -> Result<Uuid> {
        self.registry.resolve(&GitIdentity::discover(cwd)?)?
            .ok_or_else(|| anyhow!("repository has no explicit knowledge binding; use --global only for intentional global scope"))
    }
    pub fn agent(&self, name: &str) -> Option<Uuid> {
        self.agents.get(name).copied()
    }
    /// New rows have no legacy provenance to disambiguate duplicate repo IDs.
    /// Validate output compatibility before acknowledging any filesystem write.
    pub fn writable_repo(&self, cwd: &Path) -> Result<Uuid> {
        let id = self.repo(cwd)?;
        ensure!(
            self.mappings.repos.values().filter(|v| **v == id).count() == 1,
            "new note requires one explicit legacy repository mapping"
        );
        Ok(id)
    }
}
