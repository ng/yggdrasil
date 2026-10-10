use super::*;
use crate::knowledge::{
    fleet::{protocol, rollback::RollbackPlan},
    shared::SharedGit,
};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackFenceReceipt {
    pub generation: i64,
    pub hosts_sha256: String,
    pub remote_commit: String,
}
impl Journal {
    /// Freshly authenticate every local fence before committing or reconciling
    /// the global reverse fence. No host RPC runs under the SQL transition lease.
    pub async fn fence_rollback_hosts(
        &self,
        reverse: &RollbackPlan,
        pool: &sqlx::PgPool,
        identity: Option<&Path>,
    ) -> Result<RollbackFenceReceipt> {
        self.verify()?;
        reverse.expected_binding(&self.plan, self.plan.plan().participants[0].id)?;
        let prefix = format!("rollback-{}", reverse.operation());
        self.store
            .retain_artifact(&format!("{prefix}-request.json"), reverse.bytes(), false)?;
        let committed = reverse.current_fence(pool).await?.is_some();
        if !committed {
            reverse.register(pool).await?;
        }
        let mut fences = Vec::new();
        let mut failures = Vec::new();
        for host in &self.plan.plan().participants {
            match protocol::call_rollback_fence(self, reverse, host.id, identity).await {
                Ok(response) => {
                    self.verify()?;
                    self.store.retain_artifact(
                        &format!("{prefix}-host-{}.json", host.id),
                        &serde_json::to_string(response.fence())?,
                        false,
                    )?;
                    fences.push(response.fence().clone());
                }
                Err(error) => failures.push(format!("{}: {error:#}", host.name)),
            }
        }
        ensure!(
            failures.is_empty(),
            "rollback hosts still require fencing: {}",
            failures.join("; ")
        );
        let hosts = serde_json::to_string(
            &serde_json::json!({"version":1,"rollback_sha256":reverse.sha256(),"participants":fences}),
        )?;
        let name = format!("{prefix}-hosts.json");
        self.store.retain_artifact(&name, &hosts, false)?;
        let cache = self.intent.directory.join(format!("{prefix}-cache"));
        if committed {
            ensure!(cache.is_dir(), "committed rollback cache missing");
        }
        let git = SharedGit::open(&cache, self.plan.plan().shared.clone())?;
        let reconciliation = if committed {
            reverse.reconciliation(pool).await?
        } else {
            None
        };
        let selected_git = if let Some(request) = &reconciliation {
            let selected_cache = self
                .intent
                .directory
                .join(format!("{}-cache", request.prefix(reverse)));
            ensure!(
                selected_cache.is_dir(),
                "selected reconciliation cache missing"
            );
            Some(SharedGit::open(
                &selected_cache,
                self.plan.plan().shared.clone(),
            )?)
        } else {
            None
        };
        let verify_snapshot = || -> Result<()> {
            if let (Some(request), Some(selected)) = (&reconciliation, &selected_git) {
                git.verify_retained_snapshot(reverse.expected_remote_commit())?;
                selected.verify_current_snapshot(request.expected_remote_commit())?;
            } else {
                git.verify_current_snapshot(reverse.expected_remote_commit())?;
            }
            Ok(())
        };
        if committed {
            // A later recovery archive may bind this cache. Reconcile without
            // fetching or rewriting its confirmed snapshot, even on failure.
            verify_snapshot()?;
        } else {
            ensure!(
                git.refresh()?.commit == reverse.expected_remote_commit(),
                "remote changed from rollback request before SQL fencing"
            );
        }
        let saved = reverse
            .commit_fence(pool, &hosts, || {
                self.verify()?;
                ensure!(
                    self.store.read_artifact(&name)?.as_deref() == Some(hosts.as_str())
                        && self
                            .store
                            .read_artifact(&format!("{prefix}-request.json"))?
                            .as_deref()
                            == Some(reverse.bytes()),
                    "rollback evidence changed before SQL fence"
                );
                verify_snapshot()?;
                Ok(())
            })
            .await?;
        let receipt = RollbackFenceReceipt {
            generation: saved.generation,
            hosts_sha256: saved.hosts_sha256,
            remote_commit: saved.remote_commit,
        };
        self.store.retain_artifact(
            &format!("{prefix}-fenced.json"),
            &serde_json::to_string(&receipt)?,
            false,
        )?;
        Ok(receipt)
    }
}

impl Journal {
    /// Return SQL authority first, then contact every participant without holding
    /// the SQL transition lease. Partial host completion is independently resumable.
    pub async fn deselect_rollback_hosts(
        &self,
        reverse: &RollbackPlan,
        config: &crate::config::database::DeploymentConfig,
        pool: &sqlx::PgPool,
        identity: Option<&Path>,
    ) -> Result<super::SqlReturnReceipt> {
        let receipt = self
            .return_rollback_sql(reverse, config, pool, identity)
            .await?;
        let prefix = format!("rollback-{}", reverse.operation());
        let mut completed = Vec::new();
        let mut failures = Vec::new();
        for host in &self.plan.plan().participants {
            let response =
                match protocol::call_rollback_deselect(self, reverse, host.id, &receipt, identity)
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        failures.push(format!("{}: {error:#}", host.name));
                        continue;
                    }
                };
            self.verify()?;
            ensure!(
                self.store
                    .read_artifact(&format!("{prefix}-host-{}.json", host.id))?
                    .as_deref()
                    == Some(serde_json::to_string(response.fence())?.as_str()),
                "deselected fence differs from retained host evidence"
            );
            let record =
                serde_json::to_string(&serde_json::json!({"version":1,"sql_return":receipt,
                "participant":host.id,"fence":response.fence()}))?;
            let name = format!("{prefix}-deselected-{}.json", host.id);
            self.store.retain_artifact(&name, &record, false)?;
            completed.push((name, record));
        }
        ensure!(
            failures.is_empty(),
            "SQL return committed; hosts still require deselection: {}",
            failures.join("; ")
        );
        let mut tx = crate::knowledge::recovery_event::owner_transaction(pool).await?;
        let event = reverse.validate_return(&self.plan, &receipt)?;
        ensure!(
            reverse.returned_on(&mut tx, &event).await?,
            "SQL return changed before deselection seal"
        );
        self.verify()?;
        let mut records = Vec::new();
        for (name, record) in &completed {
            ensure!(
                self.store.read_artifact(name)?.as_deref() == Some(record.as_str()),
                "host deselection receipt changed"
            );
            records.push(serde_json::from_str::<serde_json::Value>(record)?);
        }
        self.store.retain_artifact(
            &format!("{prefix}-deselected.json"),
            &serde_json::to_string(
                &serde_json::json!({"version":1,"sql_return":receipt,"participants":records}),
            )?,
            false,
        )?;
        tx.commit().await?;
        Ok(receipt)
    }
}

impl Journal {
    /// Restore every host after the SQL cancellation barrier. This retains the
    /// complete census locally; it does not yet release the SQL reservation.
    pub async fn restore_rollback_hosts(
        &self,
        reverse: &RollbackPlan,
        pool: &sqlx::PgPool,
        identity: Option<&Path>,
    ) -> Result<String> {
        self.verify()?;
        reverse.expected_binding(&self.plan, self.plan.plan().participants[0].id)?;
        if let Some(saved) = reverse.cancellation_complete(pool).await? {
            let prefix = format!("rollback-{}", reverse.operation());
            self.store.retain_artifact(
                &format!("{prefix}-restored.json"),
                &saved.hosts_json,
                false,
            )?;
            self.store.retain_artifact(
                &format!("{prefix}-cancelled.json"),
                &saved.hosts_json,
                false,
            )?;
            return Ok(saved.hosts_sha256);
        }
        let receipt = reverse.begin_cancellation(pool).await?;
        let prefix = format!("rollback-{}", reverse.operation());
        self.store
            .retain_artifact(&format!("{prefix}-request.json"), reverse.bytes(), false)?;
        self.store.retain_artifact(
            &format!("{prefix}-cancellation.json"),
            &serde_json::to_string(&receipt)?,
            false,
        )?;
        let mut completed = Vec::new();
        let mut failures = Vec::new();
        for host in &self.plan.plan().participants {
            let name = format!("{prefix}-cancel-host-{}.json", host.id);
            let prior: Option<crate::knowledge::fence::LocalCancellation> = self
                .store
                .read_artifact(&name)?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?;
            let known: Option<crate::knowledge::fence::LocalFence> = self
                .store
                .read_artifact(&format!("{prefix}-host-{}.json", host.id))?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?;
            let known = known.or_else(|| prior.as_ref().and_then(|r| r.fence.clone()));
            let response = match protocol::call_rollback_cancel(
                self,
                reverse,
                host.id,
                &receipt,
                known.as_ref(),
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
            let bytes = serde_json::to_string(response.result())?;
            self.store.retain_artifact(&name, &bytes, false)?;
            completed.push((name, bytes));
        }
        ensure!(
            failures.is_empty(),
            "rollback cancelled; hosts still require restoration: {}",
            failures.join("; ")
        );
        let mut tx = crate::knowledge::recovery_event::owner_transaction(pool).await?;
        reverse.cancellation_on(&mut tx, &receipt).await?;
        self.verify()?;
        let mut records = Vec::new();
        for (name, bytes) in &completed {
            ensure!(
                self.store.read_artifact(name)?.as_deref() == Some(bytes.as_str()),
                "host cancellation evidence changed"
            );
            records.push(serde_json::from_str::<serde_json::Value>(bytes)?);
        }
        let hosts = serde_json::to_string(
            &serde_json::json!({"version":1,"rollback_sha256":reverse.sha256(),"participants":records}),
        )?;
        self.store
            .retain_artifact(&format!("{prefix}-restored.json"), &hosts, false)?;
        tx.commit().await?;
        Ok(crate::knowledge::document::digest(hosts.as_bytes()))
    }
}

impl Journal {
    /// Seal complete authenticated restoration in SQL before admitting a fresh
    /// rollback. After commit, reconcile from SQL without contacting old hosts.
    pub async fn complete_rollback_cancellation(
        &self,
        reverse: &RollbackPlan,
        pool: &sqlx::PgPool,
        identity: Option<&Path>,
    ) -> Result<String> {
        let expected = self.restore_rollback_hosts(reverse, pool, identity).await?;
        let prefix = format!("rollback-{}", reverse.operation());
        if let Some(saved) = reverse.cancellation_complete(pool).await? {
            ensure!(
                saved.hosts_sha256 == expected,
                "committed cancellation changed"
            );
            return Ok(saved.hosts_sha256);
        }
        let receipt = reverse.begin_cancellation(pool).await?;
        let hosts = self
            .store
            .read_artifact(&format!("{prefix}-restored.json"))?
            .context("restoration census missing")?;
        ensure!(
            crate::knowledge::document::digest(hosts.as_bytes()) == expected,
            "restoration census changed"
        );
        let saved = reverse
            .seal_cancellation(pool, &receipt, &hosts, || {
                self.verify()?;
                ensure!(
                    self.store
                        .read_artifact(&format!("{prefix}-restored.json"))?
                        .as_deref()
                        == Some(hosts.as_str()),
                    "restoration census changed before SQL completion"
                );
                let census: serde_json::Value = serde_json::from_str(&hosts)?;
                for host in census["participants"]
                    .as_array()
                    .context("restoration participants missing")?
                {
                    let id: uuid::Uuid =
                        serde_json::from_value(host["coordinator"]["participant"].clone())?;
                    let record = self
                        .store
                        .read_artifact(&format!("{prefix}-cancel-host-{id}.json"))?
                        .context("restored host receipt missing")?;
                    ensure!(
                        serde_json::from_str::<serde_json::Value>(&record)? == *host,
                        "restored host receipt changed"
                    );
                }
                Ok(())
            })
            .await?;
        self.verify()?;
        self.store.retain_artifact(&format!("{prefix}-cancelled.json"), &saved.hosts_json, false)
            .context("SQL cancellation completion committed; retry exact request to retain local receipt")?;
        Ok(saved.hosts_sha256)
    }
}
