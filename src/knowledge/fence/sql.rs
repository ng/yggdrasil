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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlPreparation {
    pub coordinator: CoordinatorBinding,
    pub source_generation: i64,
    pub database_id: uuid::Uuid,
    pub corpus_id: uuid::Uuid,
    pub policy: PathBuf,
    pub intent_sha256: String,
    pub fenced_sha256: String,
}

struct Host {
    policy: KnowledgeStore,
    bundle: KnowledgeStore,
    _lease: std::fs::File,
    bundle_path: PathBuf,
    desired: Intent,
    bytes: String,
    name: String,
}
impl Host {
    fn open(
        config: &KnowledgeConfig,
        binding: &Binding,
        coordinator: CoordinatorBinding,
    ) -> Result<Self> {
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
        let lease = policy.selection_lease(true)?;
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

        Ok(Self {
            policy,
            bundle,
            _lease: lease,
            bundle_path,
            desired,
            bytes,
            name,
        })
    }
    fn cancellation_name(&self) -> String {
        format!("{}-cancelled.json", self.name.trim_end_matches(".json"))
    }
    fn verify(&self, config: &KnowledgeConfig) -> Result<()> {
        self.policy.verify_root_path(&self.desired.policy)?;
        self.bundle.verify_root_path(&self.bundle_path)?;
        ensure!(
            config.knowledge_policy_dir.canonicalize()? == self.desired.policy
                && config.knowledge_dir.canonicalize()? == self.bundle_path
                && identity(&self.desired.policy)? == self.desired.policy_identity
                && identity(&self.bundle_path)? == self.desired.bundle_identity
                && self.policy.read_control("identity.json")? == self.desired.identity_policy
                && self.policy.read_control("shared.json")? == self.desired.shared_policy
                && self.policy.read_artifact(&self.name)?.as_deref() == Some(self.bytes.as_str()),
            "SQL preparation configuration, policy or journal changed"
        );
        Ok(())
    }
    fn report(&self, binding: &Binding) -> SqlPreparation {
        SqlPreparation {
            coordinator: self.desired.coordinator,
            source_generation: binding.generation,
            database_id: binding.mappings.database_id,
            corpus_id: binding.mappings.corpus_id,
            policy: self.desired.policy.clone(),
            intent_sha256: digest(self.bytes.as_bytes()),
            fenced_sha256: digest(self.desired.fenced.as_bytes()),
        }
    }
}

/// Fence subsequent local commands on an unselected SQL host. The caller must
/// first back up host-local configuration/policy and authenticate this request.
/// Existing SQL transactions still require database fencing. Both local roots
/// must exist; this neither creates a corpus nor authorizes activation.
pub fn prepare_sql(
    config: &KnowledgeConfig,
    binding: &Binding,
    coordinator: CoordinatorBinding,
) -> Result<SqlPreparation> {
    let host = Host::open(config, binding, coordinator)?;
    publish_preparation(&host, config, binding)
}

/// Prepare against the current SQL source while holding its shared generation
/// lease. Acquire local selection first to preserve local-to-database lock order.
/// The caller still authenticates the request and validates the coordinator plan;
/// this verifies database state, not a complete host census or request authority.
pub async fn prepare_sql_at_source(
    config: &KnowledgeConfig,
    binding: &Binding,
    coordinator: CoordinatorBinding,
    request_sha256: &str,
    pool: &sqlx::PgPool,
) -> Result<SqlPreparation> {
    let host = Host::open(config, binding, coordinator)?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(1497843531,1)")
        .execute(&mut *tx)
        .await?;
    let marker: (uuid::Uuid, i64, i32, String, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT database_id,generation,minimum_client,backend,corpus_id FROM public.knowledge_storage WHERE singleton"
    ).fetch_one(&mut *tx).await?;
    ensure!(
        marker.0 == binding.mappings.database_id
            && marker.1 == binding.generation
            && marker.2 > 0
            && marker.2 <= CLIENT_PROTOCOL
            && marker.3 == "sql"
            && marker.4.is_none(),
        "SQL preparation source generation is not current"
    );
    let cancelled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM public.knowledge_migration_cancellations WHERE operation_id=$1)"
    ).bind(coordinator.migration_operation).fetch_one(&mut *tx).await?;
    ensure!(
        !cancelled,
        "coordinator operation was cancelled; prepare a new request"
    );
    let registered: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.knowledge_fleet_operations WHERE operation_id=$1 AND request_sha256=$2 AND database_id=$3 AND source_generation=$4 AND corpus_id=$5 AND $6=ANY(participants))")
        .bind(coordinator.migration_operation).bind(request_sha256).bind(binding.mappings.database_id)
        .bind(binding.generation).bind(binding.mappings.corpus_id).bind(coordinator.participant)
        .fetch_one(&mut *tx).await?;
    ensure!(
        registered,
        "participant and source must match the registered coordinator plan"
    );
    let report = publish_preparation(&host, config, binding)?;
    tx.commit().await?;
    Ok(report)
}

fn publish_preparation(
    host: &Host,
    config: &KnowledgeConfig,
    binding: &Binding,
) -> Result<SqlPreparation> {
    ensure!(
        host.policy
            .read_artifact(&host.cancellation_name())?
            .is_none(),
        "SQL preparation was cancelled; an old request cannot re-fence this host"
    );
    if let Some(saved) = host.policy.read_artifact(&host.name)? {
        let _: Intent = serde_json::from_str(&saved)?;
        ensure!(
            saved == host.bytes,
            "SQL preparation request, directories or policy changed"
        );
    } else {
        ensure!(
            host.policy.read_control(SELECTION_FILE)?.is_none(),
            "SQL preparation requires an absent local selection"
        );
        host.policy
            .retain_artifact(&host.name, &host.bytes, false)?;
    }
    host.verify(config)?;
    host.policy.update_control(SELECTION_FILE, |current| {
        ensure!(
            current.is_none() || current == Some(host.desired.fenced.as_str()),
            "SQL preparation cannot replace an independent selection"
        );
        Ok((host.desired.fenced.clone(), ()))
    })?;
    host.verify(config)?;
    Ok(host.report(binding))
}

#[derive(Serialize)]
struct Cancellation {
    version: u32,
    intent_sha256: String,
    coordinator_request_sha256: String,
    target_generation: i64,
}

/// Restore original absence only after an authenticated coordinator request's
/// immutable cancellation receipt and current SQL generation are verified.
/// Keep the shared database lease through local publication, and retain a local
/// tombstone before removing the fence so a delayed preparation cannot replay it.
/// This remains a library primitive, not a complete fleet abort protocol.
pub async fn cancel_sql(
    config: &KnowledgeConfig,
    binding: &Binding,
    coordinator: CoordinatorBinding,
    coordinator_request_sha256: &str,
    pool: &sqlx::PgPool,
) -> Result<SqlPreparation> {
    ensure!(
        coordinator_request_sha256.len() == 64
            && coordinator_request_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "coordinator request SHA-256 required"
    );
    let host = Host::open(config, binding, coordinator)?;
    host.verify(config)?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(1497843531,1)")
        .execute(&mut *tx)
        .await?;
    let marker: (uuid::Uuid, i64, i32, String, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT database_id,generation,minimum_client,backend,corpus_id FROM public.knowledge_storage WHERE singleton"
    ).fetch_one(&mut *tx).await?;
    let target = binding.generation + 1;
    ensure!(
        marker.0 == binding.mappings.database_id
            && marker.1 == target
            && marker.2 > 0
            && marker.2 <= CLIENT_PROTOCOL
            && marker.3 == "sql"
            && marker.4.is_none(),
        "SQL cancellation generation is not current"
    );
    let receipt: Option<(String, uuid::Uuid, i64, i64)> = sqlx::query_as(
        "SELECT request_sha256,database_id,source_generation,target_generation FROM public.knowledge_migration_cancellations WHERE operation_id=$1"
    ).bind(coordinator.migration_operation).fetch_optional(&mut *tx).await?;
    ensure!(
        receipt
            == Some((
                coordinator_request_sha256.to_owned(),
                binding.mappings.database_id,
                binding.generation,
                target
            )),
        "matching coordinator cancellation receipt required"
    );
    host.verify(config)?;
    let current = host.policy.read_control(SELECTION_FILE)?;
    let tombstone = host.policy.read_artifact(&host.cancellation_name())?;
    let cancellation = serde_json::to_string(&Cancellation {
        version: 1,
        intent_sha256: digest(host.bytes.as_bytes()),
        coordinator_request_sha256: coordinator_request_sha256.to_owned(),
        target_generation: target,
    })?;
    if let Some(saved) = &tombstone {
        ensure!(
            saved == &cancellation,
            "local cancellation evidence changed"
        );
    }
    ensure!(
        current.as_deref() == Some(host.desired.fenced.as_str()) || current.is_none(),
        "SQL cancellation cannot replace an independent selection"
    );
    host.policy
        .retain_artifact(&host.cancellation_name(), &cancellation, false)?;
    host.verify(config)?;
    host.policy
        .remove_control(SELECTION_FILE, &host.desired.fenced)?;
    host.verify(config)?;
    tx.commit().await?;
    Ok(host.report(binding))
}
