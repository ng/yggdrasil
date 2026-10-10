//! Host finalization after committed fleet activation. The SQL receipt and live
//! generation, not caller-provided JSON or a Git commit alone, authorize selection.
use super::*;
use crate::knowledge::{
    fleet::plan::ValidatedPlan,
    shared::SharedGit,
    store::{DirectorySwapPlan, DirectorySwapState, KnowledgeBackup},
};
use anyhow::Context;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlFinalization {
    pub readiness: SqlReadiness,
    pub activation_sha256: String,
    pub selected_sha256: String,
}

/// The caller authenticates the participant transport; this function separately
/// verifies committed database authority and the exact retained host evidence.
/// No network refresh is required after activation: other hosts may already have
/// advanced the remote. Ordinary selected commands apply shared freshness rules.
pub async fn finalize_sql_backed(
    config: &crate::config::database::DeploymentConfig,
    plan: &ValidatedPlan,
    participant: uuid::Uuid,
    activation_sha256: &str,
    pool: &sqlx::PgPool,
) -> Result<SqlFinalization> {
    let p = plan.plan();
    let declared = p
        .participants
        .iter()
        .find(|h| h.id == participant)
        .context("participant missing")?;
    ensure!(
        config.knowledge_dir == declared.corpus
            && config.knowledge_policy_dir.canonicalize()? == declared.policy,
        "finalization requires the declared canonical corpus and policy paths"
    );
    let policy = KnowledgeStore::open(&declared.policy, false)?;
    let _selection = policy.selection_lease(true)?;
    policy.verify_root_path(&declared.policy)?;
    let mut tx = crate::knowledge::guard::selected_transaction(
        pool,
        p.mappings.database_id,
        p.mappings.corpus_id,
        p.source_generation + 2,
    )
    .await?;
    let mut participants = plan.registration().participants.clone();
    participants.sort_unstable();
    let (generation, saved_sha, saved_json): (i64, String, String) = sqlx::query_as(
        "SELECT a.generation,a.ready_sha256,a.ready_json FROM public.knowledge_fleet_activations a JOIN public.knowledge_fleet_operations o USING(operation_id) WHERE a.operation_id=$1 AND o.request_sha256=$2 AND o.database_id=$3 AND o.corpus_id=$4 AND o.source_generation=$5 AND o.participants=$6")
        .bind(p.operation).bind(&plan.registration().request_sha256)
        .bind(p.mappings.database_id).bind(p.mappings.corpus_id).bind(p.source_generation).bind(&participants)
        .fetch_optional(&mut *tx).await?
        .context("matching committed fleet activation missing")?;
    ensure!(
        generation == p.source_generation + 2
            && saved_sha == activation_sha256
            && digest(saved_json.as_bytes()) == saved_sha,
        "fleet activation evidence differs"
    );
    let evidence: serde_json::Value = serde_json::from_str(&saved_json)?;
    let records = evidence["participants"]
        .as_array()
        .context("activation participant set missing")?;
    let matching: Vec<_> = records
        .iter()
        .filter(|record| {
            record["readiness"]["preparation"]["coordinator"]["participant"]
                == serde_json::json!(participant)
        })
        .collect();
    ensure!(
        matching.len() == 1,
        "activation participant receipt missing or duplicated"
    );
    let ready: SqlReadiness = serde_json::from_value(matching[0]["readiness"].clone())?;
    ready.publication.validate_plan(plan)?;
    ensure!(
        serde_json::to_value(&ready.publication)? == evidence["publication"],
        "activation publication differs"
    );
    let forwarded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.knowledge_forward_receipts WHERE operation_id=$1 AND database_id=$2 AND corpus_id=$3 AND fenced_generation=$4 AND active_generation=$5 AND manifest_sha256=$6)")
        .bind(p.operation).bind(p.mappings.database_id).bind(p.mappings.corpus_id).bind(p.source_generation+1).bind(generation)
        .bind(&ready.publication.manifest_sha256).fetch_one(&mut *tx).await?;
    ensure!(
        forwarded,
        "fleet activation lacks matching forward/telemetry receipt"
    );
    let name = format!("sql-fence-{}.json", p.source_generation);
    let original_bytes = policy
        .read_artifact(&name)?
        .context("original SQL preparation missing")?;
    let original: Intent = serde_json::from_str(&original_bytes)?;
    let binding = Binding {
        version: 1,
        minimum_client: CLIENT_PROTOCOL,
        generation: p.source_generation,
        phase: Phase::Fenced,
        bundle: declared.corpus.clone(),
        mappings: serde_json::from_value(serde_json::to_value(&p.mappings)?)?,
        agents: p.agents.iter().map(|a| (a.name.clone(), a.id)).collect(),
    };
    let coordinator = CoordinatorBinding {
        migration_operation: p.operation,
        participant,
    };
    ensure!(
        original.version == 1
            && original.coordinator == coordinator
            && original.policy == declared.policy
            && original.policy_identity == identity(&declared.policy)?
            && original.fenced == serde_json::to_string(&binding)?,
        "original SQL host intent differs"
    );
    let expected = SqlPreparation {
        coordinator,
        source_generation: p.source_generation,
        database_id: p.mappings.database_id,
        corpus_id: p.mappings.corpus_id,
        policy: declared.policy.clone(),
        intent_sha256: digest(original_bytes.as_bytes()),
        fenced_sha256: digest(original.fenced.as_bytes()),
    };
    ensure!(
        ready.preparation == expected,
        "activation differs from original host preparation"
    );
    ensure!(
        policy
            .read_artifact(&format!("sql-fence-{}-cancelled.json", p.source_generation))?
            .is_none(),
        "cancelled host cannot finalize"
    );
    let staging = declared
        .corpus
        .parent()
        .context("corpus parent missing")?
        .join(format!(".ygg-fleet-{}-{}", p.operation, participant));
    ensure!(
        ready.staging == staging && ready.staging_identity == identity(&staging)?,
        "staging identity differs"
    );
    let stage = KnowledgeStore::open(&staging, false)?;
    let _stage_lease = stage.try_export_lease()?;
    ensure!(
        stage.read_artifact("ready.json")?.as_deref()
            == Some(serde_json::to_string(&ready)?.as_str()),
        "retained host readiness differs"
    );
    let swap_bytes = stage
        .read_artifact("directory-swap.json")?
        .context("directory swap evidence missing")?;
    ensure!(
        digest(swap_bytes.as_bytes()) == ready.swap_sha256,
        "directory swap evidence changed"
    );
    let swap: DirectorySwapPlan = serde_json::from_str(&swap_bytes)?;
    let mut selected: Binding = serde_json::from_value(serde_json::to_value(&binding)?)?;
    selected.phase = Phase::Okf;
    selected.generation = generation;
    let selected = serde_json::to_string(&selected)?;
    let target_shared = original
        .shared_policy
        .clone()
        .unwrap_or(serde_json::to_string(&p.shared)?);
    ensure!(
        serde_json::from_str::<serde_json::Value>(&target_shared)?
            == serde_json::to_value(&p.shared)?,
        "shared transport differs"
    );
    let ready_intent = serde_json::to_string(&serde_json::json!({"version":1,
        "request_sha256":plan.registration().request_sha256,"preparation":&expected,
        "publication":&ready.publication,"staging":&staging,"staging_identity":ready.staging_identity,
        "identity":&original.identity_policy,"shared":&target_shared,"selected":&selected}))?;
    ensure!(
        digest(ready_intent.as_bytes()) == ready.intent_sha256
            && stage.read_artifact("ready-intent.json")?.as_deref() == Some(ready_intent.as_str()),
        "readiness intent changed"
    );
    let result = SqlFinalization {
        readiness: ready.clone(),
        activation_sha256: saved_sha,
        selected_sha256: digest(selected.as_bytes()),
    };
    let current = policy.read_control(SELECTION_FILE)?;
    let mut state = swap.inspect()?;
    if current.as_deref() == Some(selected.as_str()) {
        ensure!(
            state == DirectorySwapState::CandidateInstalled
                && identity(&declared.corpus)? == ready.candidate_identity
                && policy.read_control("shared.json")?.as_deref() == Some(target_shared.as_str()),
            "selected host layout or transport changed"
        );
        // Acknowledged OKF writes and subsequent policy decisions may exist.
        // Never compare or restore the selected cache to its frozen backup.
        tx.commit().await?;
        return Ok(result);
    }
    ensure!(
        current.as_deref() == Some(original.fenced.as_str())
            && policy.read_control("identity.json")? == original.identity_policy,
        "local selection or identity policy changed before finalization"
    );
    let shared = policy.read_control("shared.json")?;
    ensure!(
        shared == original.shared_policy || shared.as_deref() == Some(target_shared.as_str()),
        "shared policy changed independently"
    );
    let manifest = crate::db::deployment_backup::verify(&declared.backup.path)?;
    ensure!(
        digest(&serde_json::to_vec(&manifest)?) == declared.backup.manifest_sha256,
        "host backup changed"
    );
    crate::db::deployment_backup::read_configuration(&declared.backup.path, &manifest)?
        .context("host backup configuration missing")?
        .verify_selection(config)?;
    let mut additions = std::collections::BTreeMap::from([
        (name.clone(), original_bytes.clone()),
        (SELECTION_FILE.to_owned(), original.fenced.clone()),
    ]);
    if original.shared_policy.is_none() {
        additions.insert("shared.json".into(), target_shared.clone());
    }
    KnowledgeBackup::verify_restored_with_new_controls(
        &declared.backup.path.join("policy"),
        &declared.policy,
        &additions,
    )?;
    let original_path = if state == DirectorySwapState::Prepared {
        declared.corpus.clone()
    } else {
        staging.join("original")
    };
    ensure!(
        identity(&original_path)? == original.bundle_identity,
        "original corpus identity changed"
    );
    KnowledgeBackup::verify_restored(&declared.backup.path.join("knowledge"), &original_path)?;
    let candidate_path = if state == DirectorySwapState::CandidateInstalled {
        declared.corpus.clone()
    } else {
        staging.join("candidate")
    };
    ensure!(
        identity(&candidate_path)? == ready.candidate_identity,
        "candidate identity changed"
    );
    let archive = staging.join("candidate-backup");
    ensure!(
        KnowledgeBackup::verify_restored(&archive, &candidate_path)?.revision
            == ready.archive_revision,
        "candidate backup changed"
    );
    let git = SharedGit::open(&candidate_path, p.shared.clone())?;
    let snapshot = git.cached()?;
    ensure!(
        snapshot.is_current && snapshot.commit == ready.publication.commit,
        "candidate publication changed"
    );
    ready.publication.verify_files(&snapshot.files)?;
    drop(git);
    while state != DirectorySwapState::CandidateInstalled {
        state = swap.advance()?;
    }
    KnowledgeBackup::verify_restored(&archive, &declared.corpus)?;
    KnowledgeBackup::verify_restored(
        &declared.backup.path.join("knowledge"),
        &staging.join("original"),
    )?;
    policy.verify_root_path(&declared.policy)?;
    stage.verify_root_path(&staging)?;
    KnowledgeBackup::verify_restored_with_new_controls(
        &declared.backup.path.join("policy"),
        &declared.policy,
        &additions,
    )?;
    ensure!(
        stage.read_artifact("directory-swap.json")?.as_deref() == Some(swap_bytes.as_str())
            && stage.read_artifact("ready-intent.json")?.as_deref() == Some(ready_intent.as_str())
            && stage.read_artifact("ready.json")?.as_deref()
                == Some(serde_json::to_string(&ready)?.as_str()),
        "retained evidence changed before local selection"
    );
    policy.update_control("shared.json", |current| {
        ensure!(
            current == original.shared_policy.as_deref() || current == Some(target_shared.as_str()),
            "shared policy changed during finalization"
        );
        Ok((target_shared.clone(), ()))
    })?;
    policy.update_control(SELECTION_FILE, |current| {
        ensure!(
            current == Some(original.fenced.as_str()) || current == Some(selected.as_str()),
            "selection changed during finalization"
        );
        Ok((selected.clone(), ()))
    })?;
    tx.commit()
        .await
        .context("host selection published; retry exact activation to verify outcome")?;
    Ok(result)
}
