//! Activation commits before host RPCs; retries after commit never republish or
//! restore the frozen export over current shared knowledge.
use super::{Journal, Publication};
use crate::{
    config::database::DeploymentConfig,
    knowledge::{
        document::digest,
        fleet::{protocol, transition},
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationReceipt {
    pub generation: i64,
    pub readiness_sha256: String,
    pub publication: Publication,
}
impl Journal {
    fn verify_activation_record(
        &self,
        saved: &transition::Activation,
    ) -> Result<ActivationReceipt> {
        self.verify()?;
        self.verify_prepared(&saved.prepared_sha256)?;
        ensure!(
            saved.backup_sha256 == self.plan.plan().source_backup.manifest_sha256
                && self.store.read_artifact("fleet-ready.json")?.as_deref()
                    == Some(saved.ready_json.as_str())
                && digest(saved.ready_json.as_bytes()) == saved.ready_sha256,
            "activation differs from retained readiness or backup"
        );
        let value: serde_json::Value = serde_json::from_str(&saved.ready_json)?;
        let publication: Publication = serde_json::from_value(value["publication"].clone())?;
        publication.validate_plan(&self.plan)?;
        ensure!(
            self.store.read_artifact("fleet-published.json")?.as_deref()
                == Some(serde_json::to_string(&publication)?.as_str()),
            "activation publication changed"
        );
        Ok(ActivationReceipt {
            generation: self.plan.plan().source_generation + 2,
            readiness_sha256: saved.ready_sha256.clone(),
            publication,
        })
    }
    pub async fn activate_hosts(
        &self,
        config: &DeploymentConfig,
        pool: &sqlx::PgPool,
        identity: Option<&Path>,
    ) -> Result<ActivationReceipt> {
        self.verify()?;
        let saved =
            if let Some(saved) = transition::activation(self.plan.registration(), pool).await? {
                // SQL is already active. No old-source parity or remote-tip check:
                // selected participants may have acknowledged newer shared writes.
                self.source_backup(config)?.verify()?;
                saved
            } else {
                let expected = self.ready_hosts(config, pool, identity).await?;
                let ready = self
                    .store
                    .read_artifact("fleet-ready.json")?
                    .context("readiness missing")?;
                ensure!(
                    digest(ready.as_bytes()) == expected,
                    "readiness changed before activation"
                );
                let prepared = digest(
                    self.store
                        .read_artifact("fleet-prepared.json")?
                        .context("preparation missing")?
                        .as_bytes(),
                );
                let backup = self.source_backup(config)?;
                let manifest = self.verified_export()?;
                transition::activate(
                    self.plan.registration(),
                    pool,
                    &backup,
                    &prepared,
                    &ready,
                    &manifest,
                    || {
                        self.verify_prepared(&prepared)?;
                        ensure!(
                            self.store.read_artifact("fleet-ready.json")?.as_deref()
                                == Some(ready.as_str()),
                            "readiness changed before activation commit"
                        );
                        self.verify_publication_for_abort()
                    },
                )
                .await?
            };
        let receipt = self.verify_activation_record(&saved)?;
        self.store.retain_artifact(
            "fleet-activated.json",
            &serde_json::to_string(&receipt)?,
            false,
        )?;
        Ok(receipt)
    }
    pub async fn finalize_hosts(
        &self,
        config: &DeploymentConfig,
        pool: &sqlx::PgPool,
        identity: Option<&Path>,
    ) -> Result<ActivationReceipt> {
        let activation = self.activate_hosts(config, pool, identity).await?;
        let ready: serde_json::Value = serde_json::from_str(
            &self
                .store
                .read_artifact("fleet-ready.json")?
                .context("readiness missing")?,
        )?;
        let expected = ready["participants"]
            .as_array()
            .context("readiness participant set missing")?;
        let mut failures = Vec::new();
        let mut completed = Vec::new();
        for host in &self.plan.plan().participants {
            let response = match protocol::call_finalize(
                self,
                host.id,
                &activation.publication,
                &activation.readiness_sha256,
                identity,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => {
                    failures.push(format!("{}: {error:#}", host.name));
                    continue;
                }
            };
            self.verify()?;
            let ready = response
                .readiness()
                .context("finalization readiness missing")?;
            let expected = expected
                .iter()
                .find(|record| {
                    record["readiness"]["preparation"]["coordinator"]["participant"]
                        == serde_json::json!(host.id)
                })
                .context("sealed participant missing")?;
            ensure!(
                serde_json::to_value(ready)? == expected["readiness"],
                "finalized host differs from activated readiness"
            );
            let record = serde_json::json!({"version":1,"operation":self.plan.plan().operation,
                "activation_sha256":activation.readiness_sha256,"participant":host.id,
                "selection_sha256":response.selection_sha256().context("selected binding evidence missing")?});
            self.store.retain_artifact(
                &format!("finalized-{}.json", host.id),
                &serde_json::to_string(&record)?,
                false,
            )?;
            completed.push(record);
        }
        ensure!(
            failures.is_empty(),
            "fleet activation committed; hosts still require finalization: {}",
            failures.join("; ")
        );
        transition::with_activation(self.plan.registration(), pool, &activation.readiness_sha256, |saved| {
            ensure!(self.verify_activation_record(saved)? == activation, "activation changed before finalization seal");
            self.store.retain_artifact("fleet-finalized.json", &serde_json::to_string(&serde_json::json!({"version":1,"activation":activation,"participants":completed}))?, false)
        }).await?;
        Ok(activation)
    }
}
