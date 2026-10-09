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
    generation: i64,
    registry: IdentityRegistry,
    agents: BTreeMap<String, Uuid>,
    policy: KnowledgeStore,
    pub(super) user: String,
    pub agent_context: bool,
    _selection_lease: File,
}
impl Context {
    pub fn from_environment(env: Environment) -> Result<Option<Self>> {
        let (config, env) = KnowledgeConfig::load(env)?;
        let user = env
            .get("YGG_USER")
            .filter(|value| !value.is_empty())
            .cloned()
            .unwrap_or_else(|| {
                std::process::Command::new("whoami")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "default".into())
            });
        let mut context = Self::open(&config, &user)?;
        if let Some(context) = &mut context {
            context.agent_context = env.contains_key("YGG_AGENT_NAME");
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
            user.clone(),
        )?;
        Ok(Some(Self {
            service,
            default_agent_name: "ygg".into(),
            mappings: binding.mappings,
            generation: binding.generation,
            registry,
            agents: binding.agents,
            policy,
            user,
            agent_context: false,
            _selection_lease: lease,
        }))
    }
    pub(super) fn session_store(&self) -> Result<KnowledgeStore> {
        self.policy.private_child(".sessions")
    }
    pub fn approver(&self, explicit_agent: Option<&str>) -> Result<super::service::Approver> {
        if explicit_agent.is_some() || self.agent_context {
            let name = explicit_agent.unwrap_or(&self.default_agent_name);
            Ok(super::service::Approver::Agent(
                self.agent(name)
                    .ok_or_else(|| anyhow!("approval agent has no explicit identity binding"))?,
            ))
        } else {
            Ok(super::service::Approver::Human(None))
        }
    }
    /// Read-only last-known operational totals, outside authoritative documents.
    /// Cutover must supply migrated baselines; telemetry refresh is independent.
    pub fn usage_snapshot(&self) -> Result<UsageSnapshot> {
        let read = |name| -> Result<UsageSnapshot> {
            let snapshot = match self.policy.read_artifact(name)? {
                Some(text) => serde_json::from_str::<UsageSnapshot>(&text)?,
                None => UsageSnapshot {
                    version: 1,
                    corpus_id: self.mappings.corpus_id,
                    totals: BTreeMap::new(),
                },
            };
            ensure!(
                snapshot.version == 1 && snapshot.corpus_id == self.mappings.corpus_id,
                "usage snapshot corpus/version mismatch"
            );
            for (id, usage) in &snapshot.totals {
                ensure!(
                    *id == usage.document_id && usage.corpus_id == snapshot.corpus_id,
                    "usage snapshot identity mismatch"
                );
            }
            Ok(snapshot)
        };
        // Baselines belong to the validated migration receipt, not an optional
        // telemetry cache. Never silently invent migrated counters when missing.
        let mut baseline = read("usage-baseline.json")?;
        match read("usage-snapshot.json") {
            Ok(cached) => {
                for (id, value) in cached.totals {
                    let regressed = baseline.totals.get(&id).is_some_and(|old| {
                        value.applied_count < old.applied_count
                            || value.last_applied_at < old.last_applied_at
                    });
                    if regressed {
                        eprintln!("knowledge: ignored usage cache older than migration baseline");
                    } else {
                        baseline.totals.insert(id, value);
                    }
                }
            }
            Err(_) => {
                eprintln!("knowledge: usage cache unavailable; using recorded migration baselines")
            }
        }
        Ok(baseline)
    }
    pub fn repo(&self, cwd: &Path) -> Result<Uuid> {
        self.registry.resolve(&GitIdentity::discover(cwd)?)?
            .ok_or_else(|| anyhow!("repository has no explicit knowledge binding; use --global only for intentional global scope"))
    }
    /// Connected task claims use the task's database scope, never checkout scope.
    /// Hold both this context and the returned transaction through emission.
    pub async fn task_scope(
        &self,
        pool: &sqlx::PgPool,
        legacy_repo: Uuid,
    ) -> Result<(Uuid, sqlx::Transaction<'static, sqlx::Postgres>)> {
        let transaction = super::guard::selected_transaction(
            pool,
            self.mappings.database_id,
            self.mappings.corpus_id,
            self.generation,
        )
        .await?;
        let repo = self
            .registry
            .from_legacy(self.mappings.database_id, legacy_repo)?;
        ensure!(
            self.mappings.repos.get(&legacy_repo) == Some(&repo),
            "task repository has no matching selection binding"
        );
        Ok((repo, transaction))
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
            "new knowledge requires one explicit legacy repository mapping"
        );
        Ok(id)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSnapshot {
    pub version: u32,
    pub corpus_id: Uuid,
    pub totals: BTreeMap<Uuid, super::legacy::Usage>,
}
impl UsageSnapshot {
    pub fn for_document(&self, doc: &super::document::Document) -> Result<super::legacy::Usage> {
        let id = doc
            .profile()?
            .ok_or_else(|| anyhow!("knowledge profile required"))?
            .id;
        if let Some(usage) = self.totals.get(&id) {
            return Ok(usage.clone());
        }
        ensure!(
            !super::legacy::is_imported(doc)?,
            "migrated learning {id} has no recorded usage baseline; repair cutover metadata"
        );
        Ok(super::legacy::Usage {
            corpus_id: self.corpus_id,
            document_id: id,
            applied_count: 0,
            last_applied_at: None,
        })
    }
}
