//! Host-side staging only. This receipt does not authenticate its caller or
//! authorize activation; the fleet RPC/coordinator must bind it to a live request.
use super::*;
use crate::knowledge::{
    export::Manifest,
    fleet::{journal::Publication, plan::ValidatedPlan},
    inventory,
    shared::SharedGit,
    source_backup::SourceBackup,
    store::{DirectorySwapPlan, Key, KnowledgeBackup},
};
use anyhow::Context;
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlReadiness {
    pub preparation: SqlPreparation,
    pub publication: Publication,
    pub staging: PathBuf,
    pub staging_identity: (u64, u64),
    pub candidate_identity: (u64, u64),
    pub archive_revision: String,
    pub intent_sha256: String,
    pub swap_sha256: String,
}
fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
async fn verify_manifest(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    plan: &ValidatedPlan,
    publication: &Publication,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<Manifest> {
    publication.verify_files(files)?;
    let p = plan.plan();
    let bytes = files
        .get(&format!("yggdrasil-migrations/{}.json", p.operation))
        .context("published migration manifest missing")?;
    ensure!(
        digest(bytes) == publication.manifest_sha256,
        "published manifest digest differs"
    );
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    ensure!(
        manifest.version == 1
            && manifest.database_id == p.mappings.database_id
            && manifest.corpus_id == p.mappings.corpus_id
            && manifest.generation == p.source_generation + 1
            && manifest.mappings == serde_json::to_value(&p.mappings)?,
        "published manifest differs from fleet source"
    );
    let expected: BTreeMap<_, _> = manifest.entries.iter().map(|e| (e.key.id, e)).collect();
    ensure!(
        expected.len() == manifest.entries.len(),
        "duplicate manifest document UUID"
    );
    let paths: std::collections::BTreeSet<_> = manifest
        .entries
        .iter()
        .map(|e| e.key.relative_path().to_string_lossy().into_owned())
        .collect();
    for (path, bytes) in files {
        if !paths.contains(path) {
            let knowledge = path.starts_with("global/")
                || path.starts_with("repos/")
                || (path.ends_with(".md")
                    && std::str::from_utf8(bytes)
                        .ok()
                        .is_some_and(|s| crate::knowledge::document::Document::parse(s).is_ok()));
            ensure!(!knowledge, "unlisted published knowledge document: {path}");
        }
    }
    let mut observed = 0;
    let report = inventory::visit_on(tx, Some(&p.mappings), |_, row, document, usage| {
        let key = Key::from_document(document)?;
        let entry = expected
            .get(&key.id)
            .context("published manifest omits a source document")?;
        let name = key.relative_path();
        let bytes = files
            .get(name.to_str().context("non-UTF8 document path")?)
            .context("published source document missing")?;
        ensure!(
            entry.key == key
                && entry.source_digest == row.source_digest
                && Some(&entry.document_digest) == row.document_digest.as_ref()
                && entry.usage.as_ref() == usage
                && digest(bytes) == entry.document_digest
                && document.serialize()?.as_bytes() == bytes,
            "published document or metadata differs from frozen SQL source"
        );
        observed += 1;
        Ok(())
    })
    .await?;
    ensure!(
        report.rows_verified
            && observed == expected.len()
            && report.source.backend == "fenced"
            && report.source.generation == manifest.generation
            && report.source.database_id == manifest.database_id
            && report.source.corpus_id == Some(manifest.corpus_id),
        "readiness requires a complete unchanged fenced source"
    );
    Ok(manifest)
}

/// Stage a verified candidate beside the original corpus on the same filesystem.
/// Original corpus and policy remain unchanged and locally fenced. Caller must
/// authenticate this plan/publication; this library result alone is not readiness
/// authority and cannot finalize a host or alter the database storage marker.
pub async fn ready_sql_backed(
    config: &crate::config::database::DeploymentConfig,
    plan: &ValidatedPlan,
    participant: uuid::Uuid,
    publication: &Publication,
    pool: &sqlx::PgPool,
) -> Result<SqlReadiness> {
    publication.validate_plan(plan)?;
    let p = plan.plan();
    let declared = p
        .participants
        .iter()
        .find(|h| h.id == participant)
        .context("participant missing")?;
    ensure!(
        config.knowledge_dir.canonicalize()? == declared.corpus
            && config.knowledge_policy_dir.canonicalize()? == declared.policy,
        "readiness request differs from actual host paths"
    );
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
    let local = local_config(config);
    let host = Host::open(&local, &binding, coordinator)?;
    host.verify(&local)?;
    let mut tx = inspection_lease(
        &host,
        &binding,
        coordinator,
        &plan.registration().request_sha256,
        &p.source_backup.manifest_sha256,
        pool,
    )
    .await?;
    verify_host_backup(
        &host,
        config,
        &binding,
        &declared.backup.path,
        &declared.backup.manifest_sha256,
        None,
    )?;
    let source = SourceBackup::open(
        &declared.backup.path,
        p.mappings.database_id,
        p.source_generation,
    )?;
    ensure!(
        source.digest() == declared.backup.manifest_sha256,
        "participant source backup changed"
    );
    source.verify_on(&mut tx).await?;
    ensure!(
        host.desired
            .identity_policy
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()?
            == Some(serde_json::to_value(&declared.identities)?),
        "original host identity/trust policy differs from declared target"
    );
    let shared = serde_json::to_string(&p.shared)?;
    if let Some(original) = &host.desired.shared_policy {
        ensure!(
            serde_json::from_str::<serde_json::Value>(original)?
                == serde_json::to_value(&p.shared)?,
            "original shared policy conflicts with target transport"
        );
    }
    let mut selected: Binding = serde_json::from_value(serde_json::to_value(&binding)?)?;
    selected.phase = Phase::Okf;
    selected.generation += 2;
    let staging = declared
        .corpus
        .parent()
        .context("corpus parent missing")?
        .join(format!(".ygg-fleet-{}-{}", p.operation, participant));
    ensure!(
        !staging.starts_with(&declared.policy)
            && !declared.policy.starts_with(&staging)
            && !staging.starts_with(&declared.backup.path)
            && !declared.backup.path.starts_with(&staging),
        "readiness staging overlaps policy or backup"
    );
    let stage = KnowledgeStore::open(&staging, true)?;
    let _lease = stage.try_export_lease()?;
    stage.verify_root_path(&staging)?;
    let staging_identity = identity(&staging)?;
    ensure!(
        staging_identity.0 == host.desired.bundle_identity.0,
        "candidate staging and original corpus must share a filesystem"
    );
    let preparation = host.report(&binding);
    let intent = serde_json::to_string(&serde_json::json!({"version":1,
        "request_sha256":plan.registration().request_sha256,
        "preparation":&preparation,"publication":publication,
        "staging":&staging,"staging_identity":staging_identity,
        "identity":&host.desired.identity_policy,
        "shared":host.desired.shared_policy.as_deref().unwrap_or(&shared),
        "selected":serde_json::to_string(&selected)?,
    }))?;
    if stage.read_artifact("ready-intent.json")?.is_none() {
        for entry in std::fs::read_dir(&staging)? {
            ensure!(
                entry?.file_name() == ".export.lock",
                "unowned readiness staging directory is not empty"
            );
        }
    }
    stage.retain_artifact("ready-intent.json", &intent, false)?;
    let candidate = staging.join("candidate");
    let archive = staging.join("candidate-backup");
    let saved: Option<SqlReadiness> = stage
        .read_artifact("ready.json")?
        .map(|s| serde_json::from_str(&s))
        .transpose()?;
    if let Some(saved) = &saved {
        ensure!(
            saved.preparation == preparation
                && saved.publication == *publication
                && saved.staging == staging
                && saved.staging_identity == staging_identity
                && saved.candidate_identity == identity(&candidate)?
                && saved.intent_sha256 == digest(intent.as_bytes()),
            "retained readiness or candidate identity changed"
        );
    }
    let archived = exists(&archive)?;
    ensure!(
        saved.is_none() || archived,
        "retained readiness backup missing"
    );
    if archived {
        KnowledgeBackup::verify_restored(&archive, &candidate)?;
    }
    let git = SharedGit::open(&candidate, p.shared.clone())?;
    let snapshot = if archived {
        git.verify_current_snapshot(&publication.commit)?
    } else {
        let snapshot = git.refresh()?;
        ensure!(
            snapshot.commit == publication.commit,
            "remote tip differs from planned publication"
        );
        snapshot
    };
    verify_manifest(&mut tx, plan, publication, &snapshot.files).await?;
    let capture = if archived {
        KnowledgeBackup::verify_restored(&archive, &candidate)?
    } else {
        KnowledgeStore::open(&candidate, false)?.backup(&archive)?
    };
    let swap = DirectorySwapPlan::capture(&declared.corpus, &staging)?;
    let swap_bytes = serde_json::to_string(&swap)?;
    let swap_sha256 = digest(swap_bytes.as_bytes());
    if let Some(saved) = &saved {
        ensure!(
            saved.swap_sha256 == swap_sha256
                && stage.read_artifact("directory-swap.json")?.as_deref()
                    == Some(swap_bytes.as_str()),
            "retained directory swap plan changed or is missing"
        );
    }
    stage.retain_artifact("directory-swap.json", &swap_bytes, false)?;
    let ready = SqlReadiness {
        preparation,
        publication: publication.clone(),
        staging: staging.clone(),
        staging_identity,
        candidate_identity: identity(&candidate)?,
        swap_sha256,
        archive_revision: capture.revision,
        intent_sha256: digest(intent.as_bytes()),
    };
    if let Some(saved) = saved {
        ensure!(saved == ready, "readiness backup or receipt changed");
    }
    let snapshot = git.verify_current_snapshot(&publication.commit)?;
    publication.verify_files(&snapshot.files)?;
    KnowledgeBackup::verify_restored(&archive, &candidate)?;
    host.verify(&local)?;
    verify_host_backup(
        &host,
        config,
        &binding,
        &declared.backup.path,
        &declared.backup.manifest_sha256,
        None,
    )?;
    stage.verify_root_path(&staging)?;
    ensure!(
        DirectorySwapPlan::capture(&declared.corpus, &staging)? == swap
            && stage.read_artifact("directory-swap.json")?.as_deref() == Some(swap_bytes.as_str()),
        "directory swap evidence changed before readiness completion"
    );
    stage.retain_artifact("ready-intent.json", &intent, false)?;
    stage.retain_artifact("ready.json", &serde_json::to_string(&ready)?, false)?;
    tx.commit()
        .await
        .context("host readiness outcome uncertain; retry the exact request")?;
    Ok(ready)
}
