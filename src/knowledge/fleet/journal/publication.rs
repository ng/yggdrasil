//! Conditional publication of the verified fleet export. This never activates
//! hosts; a confirmed Git commit alone is not database authority.
use super::Journal;
use crate::knowledge::{
    document::{Document, digest},
    export::{self, Manifest},
    shared::SharedGit,
    store::KnowledgeStore,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

const STARTED: &str = "fleet-publication-started.json";
const INTENT: &str = "fleet-publication-intent.json";
const COMPLETE: &str = "fleet-published.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    version: u32,
    operation: uuid::Uuid,
    request_sha256: String,
    base: String,
    pub manifest_sha256: String,
    desired_sha256: String,
    pub commit: String,
}
impl Publication {
    pub(crate) fn validate_plan(
        &self,
        plan: &crate::knowledge::fleet::plan::ValidatedPlan,
    ) -> Result<()> {
        let hex = |s: &str, n| {
            s.len() == n
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        ensure!(
            self.version == 1
                && self.operation == plan.plan().operation
                && self.request_sha256 == plan.registration().request_sha256
                && self.base == plan.plan().expected_remote_commit
                && hex(&self.manifest_sha256, 64)
                && hex(&self.desired_sha256, 64)
                && (hex(&self.commit, 40) || hex(&self.commit, 64)),
            "publication differs from the registered fleet plan"
        );
        Ok(())
    }
    pub(crate) fn verify_files(&self, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
        let hashes: BTreeMap<_, _> = files
            .iter()
            .map(|(path, bytes)| (path, digest(bytes)))
            .collect();
        ensure!(
            digest(&serde_json::to_vec(&hashes)?) == self.desired_sha256,
            "publication tree differs from coordinator receipt"
        );
        Ok(())
    }
}
impl Journal {
    fn verified_export(&self) -> Result<Manifest> {
        self.verify()?;
        let manifest = export::verify(&self.intent.directory.join("stage"))?;
        let plan = self.plan()?.plan();
        ensure!(
            manifest.database_id == plan.mappings.database_id
                && manifest.generation == plan.source_generation + 1
                && manifest.corpus_id == plan.mappings.corpus_id
                && manifest.mappings == serde_json::to_value(&plan.mappings)?,
            "publication export differs from fleet plan"
        );
        let prepared = self
            .store
            .read_artifact("fleet-prepared.json")?
            .context("complete preparation evidence missing")?;
        let expected = self.export_bytes(
            &manifest,
            &digest(prepared.as_bytes()),
            &plan.source_backup.manifest_sha256,
        )?;
        ensure!(
            self.store.read_artifact("fleet-export.json")?.as_deref() == Some(expected.as_str()),
            "retained fleet export evidence changed"
        );
        Ok(manifest)
    }
    fn publication_start(&self, manifest: &Manifest) -> Result<String> {
        Ok(serde_json::to_string(&serde_json::json!({"version":1,
            "operation":self.plan()?.plan().operation,
            "request_sha256":self.intent.request_sha256,
            "base":self.plan.plan().expected_remote_commit,
            "manifest_sha256":digest(&serde_json::to_vec(manifest)?),
        }))?)
    }
    fn desired_snapshot(
        &self,
        git: &SharedGit,
        manifest: &Manifest,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        let store = KnowledgeStore::open(&self.intent.directory.join("stage"), false)?;
        let mut documents = BTreeMap::new();
        for entry in &manifest.entries {
            let document = store
                .get(entry.key)?
                .context("staged export document missing")?;
            let bytes = document.document.serialize()?.into_bytes();
            ensure!(
                document.revision == entry.document_digest
                    && digest(&bytes) == entry.document_digest,
                "staged document bytes changed before publication"
            );
            documents.insert(
                entry
                    .key
                    .relative_path()
                    .to_str()
                    .context("non-UTF8 document path")?
                    .to_owned(),
                bytes,
            );
        }
        let mut desired = git.snapshot_files(&self.plan.plan().expected_remote_commit)?;
        for (path, bytes) in &desired {
            if let Some(exported) = documents.get(path) {
                ensure!(
                    bytes == exported,
                    "planned remote contains a conflicting knowledge document: {path}"
                );
            } else {
                let knowledge = path.starts_with("global/")
                    || path.starts_with("repos/")
                    || (path.ends_with(".md")
                        && std::str::from_utf8(bytes)
                            .ok()
                            .is_some_and(|s| Document::parse(s).is_ok()));
                ensure!(
                    !knowledge,
                    "planned remote contains knowledge outside the export: {path}"
                );
            }
        }
        desired.extend(documents);
        let manifest_path = format!("yggdrasil-migrations/{}.json", self.plan.plan().operation);
        let bytes = serde_json::to_vec(manifest)?;
        if let Some(existing) = desired.get(&manifest_path) {
            ensure!(
                existing == &bytes,
                "remote migration manifest conflicts with this operation"
            );
        }
        desired.insert(manifest_path, bytes);
        ensure!(
            self.verified_export()? == *manifest,
            "export changed while building publication"
        );
        Ok(desired)
    }
    fn publication(
        &self,
        manifest: &Manifest,
        desired: &BTreeMap<String, Vec<u8>>,
        commit: &str,
    ) -> Result<Publication> {
        let hashes: BTreeMap<_, _> = desired
            .iter()
            .map(|(path, bytes)| (path, digest(bytes)))
            .collect();
        Ok(Publication {
            version: 1,
            operation: self.plan.plan().operation,
            request_sha256: self.intent.request_sha256.clone(),
            base: self.plan.plan().expected_remote_commit.clone(),
            manifest_sha256: digest(&serde_json::to_vec(manifest)?),
            desired_sha256: digest(&serde_json::to_vec(&hashes)?),
            commit: commit.to_owned(),
        })
    }
    fn retain_publication(&self, publication: &Publication) -> Result<()> {
        self.verify()?;
        self.store
            .retain_artifact(INTENT, &serde_json::to_string(publication)?, false)
    }
    /// Publish one exact complete tree while this fleet's SQL fence is current.
    /// Local participants stay fenced. Retry confirms/reuses the retained commit.
    pub async fn publish_hosts(
        &self,
        config: &crate::config::database::DeploymentConfig,
        pool: &sqlx::PgPool,
        ssh_identity: Option<&Path>,
    ) -> Result<Publication> {
        let manifest = self.stage_hosts(config, pool, ssh_identity).await?;
        // Refresh host bindings again after potentially lengthy export work.
        self.fence_hosts(config, pool, ssh_identity).await?;
        let backup = self.source_backup(config)?;
        let prepared = self
            .store
            .read_artifact("fleet-prepared.json")?
            .context("complete preparation evidence missing")?;
        let prepared = digest(prepared.as_bytes());
        super::super::transition::with_fenced_source(
            self.plan.registration(),
            pool,
            &backup,
            &prepared,
            || {
                self.verify_prepared(&prepared)?;
                ensure!(
                    self.verified_export()? == manifest,
                    "export changed before publication"
                );
                self.store
                    .retain_artifact(STARTED, &self.publication_start(&manifest)?, false)?;
                let git = SharedGit::open(
                    &self.intent.directory.join("publication-cache"),
                    self.plan.plan().shared.clone(),
                )?;
                git.refresh()?;
                let desired = self.desired_snapshot(&git, &manifest)?;
                let prior = self.store.read_artifact(INTENT)?;
                let pending = git.pending_info()?;
                let receipt = if let Some(bytes) = prior {
                    let saved: Publication = serde_json::from_str(&bytes)?;
                    self.retain_publication(&self.publication(
                        &manifest,
                        &desired,
                        &saved.commit,
                    )?)?;
                    git.resume_snapshot(
                        &self.plan.plan().expected_remote_commit,
                        &saved.commit,
                        &desired,
                    )?
                } else if let Some(pending) = pending {
                    // A crash can occur between transport's pending write and our
                    // callback. Bind that candidate before allowing its exact retry.
                    ensure!(
                        pending.exact_base
                            && pending.base == self.plan.plan().expected_remote_commit,
                        "transport pending state differs from fleet base"
                    );
                    self.retain_publication(&self.publication(
                        &manifest,
                        &desired,
                        &pending.commit,
                    )?)?;
                    git.resume_snapshot(&pending.base, &pending.commit, &desired)?
                } else {
                    git.replace_snapshot_at(
                        &self.plan.plan().expected_remote_commit,
                        &desired,
                        &mut |commit| {
                            self.verify_prepared(&prepared)?;
                            ensure!(
                                self.verified_export()? == manifest,
                                "export changed before push"
                            );
                            self.retain_publication(&self.publication(&manifest, &desired, commit)?)
                        },
                    )?
                };
                let publication = self.publication(&manifest, &desired, &receipt.commit)?;
                self.retain_publication(&publication)?; // Also records a verified no-op.
                let current = git.refresh()?;
                ensure!(
                    current.commit == publication.commit && current.files == desired,
                    "published fleet commit is not the exact current remote snapshot"
                );
                self.verify_prepared(&prepared)?;
                ensure!(
                    self.verified_export()? == manifest,
                    "export changed after push"
                );
                self.store.retain_artifact(
                    COMPLETE,
                    &serde_json::to_string(&publication)?,
                    false,
                )?;
                Ok(publication)
            },
        )
        .await
    }
    /// Obtain fresh authenticated readiness from every declared host. This seals
    /// evidence only; it does not activate SQL or finalize any local selection.
    pub async fn ready_hosts(
        &self,
        config: &crate::config::database::DeploymentConfig,
        pool: &sqlx::PgPool,
        ssh_identity: Option<&Path>,
    ) -> Result<String> {
        let publication = self.publish_hosts(config, pool, ssh_identity).await?;
        let prepared_bytes = self
            .store
            .read_artifact("fleet-prepared.json")?
            .context("preparation evidence missing")?;
        let prepared = digest(prepared_bytes.as_bytes());
        let prepared_records: Vec<serde_json::Value> = serde_json::from_str(&prepared_bytes)?;
        let mut records = Vec::new();
        for host in &self.plan()?.plan().participants {
            let response =
                super::super::protocol::call_ready(self, host.id, &publication, ssh_identity)
                    .await?;
            self.verify()?;
            let ready = response
                .readiness()
                .context("authenticated readiness missing")?;
            let expected = prepared_records
                .iter()
                .find(|record| {
                    record["receipt"]["coordinator"]["participant"] == serde_json::json!(host.id)
                })
                .context("participant preparation missing from sealed set")?;
            ensure!(
                serde_json::to_value(&ready.preparation)? == expected["receipt"],
                "host readiness no longer matches its sealed preparation"
            );
            let record = serde_json::json!({"version":1,"operation":self.plan.plan().operation,
                "request_sha256":self.intent.request_sha256,"readiness":ready});
            self.store.retain_artifact(
                &format!("ready-{}.json", host.id),
                &serde_json::to_string(&record)?,
                false,
            )?;
            records.push(record);
        }
        let backup = self.source_backup(config)?;
        super::super::transition::with_fenced_source(
            self.plan.registration(),
            pool,
            &backup,
            &prepared,
            || {
                self.verify_prepared(&prepared)?;
                ensure!(
                    self.store.read_artifact(COMPLETE)?.as_deref()
                        == Some(serde_json::to_string(&publication)?.as_str()),
                    "publication completion changed before readiness seal"
                );
                self.verify_publication_for_abort()?;
                let bytes = serde_json::to_string(&serde_json::json!({"version":1,
                "publication":publication,"participants":records}))?;
                self.store
                    .retain_artifact("fleet-ready.json", &bytes, false)?;
                Ok(digest(bytes.as_bytes()))
            },
        )
        .await
    }

    /// Called under the SQL recovery lease. Never push, reset or discard a draft.
    pub(super) fn verify_publication_for_abort(&self) -> Result<()> {
        let started = self.store.read_artifact(STARTED)?;
        let intent = self.store.read_artifact(INTENT)?;
        let completed = self.store.read_artifact(COMPLETE)?;
        let cache = self.intent.directory.join("publication-cache");
        let Some(started) = started else {
            ensure!(
                intent.is_none() && completed.is_none() && !cache.try_exists()?,
                "publication evidence missing before abort"
            );
            return Ok(());
        };
        let manifest = self.verified_export()?;
        ensure!(
            started == self.publication_start(&manifest)?,
            "publication start evidence changed"
        );
        let git = SharedGit::open(&cache, self.plan.plan().shared.clone())?;
        let pending = git.pending_info()?;
        if intent.is_none() && pending.is_none() {
            ensure!(completed.is_none(), "publication completion lacks intent");
            return Ok(()); // No push was prepared.
        }
        let current = git.refresh()?;
        let desired = self.desired_snapshot(&git, &manifest)?;
        let candidate = if let Some(bytes) = &intent {
            let saved: Publication = serde_json::from_str(bytes)?;
            ensure!(
                saved == self.publication(&manifest, &desired, &saved.commit)?,
                "publication intent changed"
            );
            saved.commit
        } else {
            pending.as_ref().unwrap().commit.clone()
        };
        ensure!(
            git.snapshot_files(&candidate)? == desired,
            "candidate publication bytes changed"
        );
        if let Some(pending) = &pending {
            ensure!(
                pending.exact_base
                    && pending.base == self.plan.plan().expected_remote_commit
                    && pending.commit == candidate,
                "pending publication changed before abort"
            );
        }
        if let Some(completed) = completed {
            ensure!(
                Some(&completed) == intent.as_ref() && current.commit == candidate,
                "confirmed publication changed before abort"
            );
        } else {
            ensure!(
                current.commit == candidate
                    || (pending.is_some()
                        && current.commit == self.plan.plan().expected_remote_commit),
                "remote publication state changed before abort"
            );
        }
        if current.commit == candidate {
            ensure!(
                current.files == desired,
                "published snapshot changed before abort"
            );
        }
        self.verify()
    }
}
