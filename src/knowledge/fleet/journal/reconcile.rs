use super::*;
use crate::knowledge::{
    document::digest,
    fleet::{
        protocol,
        rollback::{ReconciliationPlan, RollbackPlan},
    },
    shared::SharedGit,
};

impl Journal {
    /// Explicitly select a descendant snapshot after the reverse fence. Every
    /// host is reauthenticated, while all earlier snapshot archives stay frozen.
    pub async fn reconcile_rollback_remote(
        &self,
        reverse: &RollbackPlan,
        request: &ReconciliationPlan,
        pool: &sqlx::PgPool,
        identity: Option<&Path>,
    ) -> Result<String> {
        self.verify()?;
        ReconciliationPlan::parse(reverse, request.bytes())?;
        reverse.expected_binding(&self.plan, self.plan.plan().participants[0].id)?;
        let fence = reverse
            .current_fence(pool)
            .await?
            .context("reconciliation requires committed reverse fence")?;
        let prefix = request.prefix(reverse);
        self.store
            .retain_artifact(&format!("{prefix}-request.json"), request.bytes(), false)?;
        let mut fences = Vec::new();
        for host in &self.plan.plan().participants {
            let response = protocol::call_rollback_fence(self, reverse, host.id, identity).await?;
            let bytes = serde_json::to_string(response.fence())?;
            self.store.retain_artifact(
                &format!("{prefix}-host-{}.json", host.id),
                &bytes,
                false,
            )?;
            fences.push(response.fence().clone());
        }
        let hosts = serde_json::to_string(
            &serde_json::json!({"version":1,"rollback_sha256":reverse.sha256(),"participants":fences}),
        )?;
        ensure!(
            hosts == fence.hosts_json && digest(hosts.as_bytes()) == fence.hosts_sha256,
            "reconciliation host fences differ from committed census"
        );
        let current = reverse.reconciliation(pool).await?;
        let selected = current
            .as_ref()
            .is_some_and(|c| c.sha256() == request.sha256());
        let cache = self.intent.directory.join(format!("{prefix}-cache"));
        let prepared_name = format!("{prefix}-prepared.json");
        let prepared = self.store.read_artifact(&prepared_name)?;
        if selected || prepared.is_some() {
            ensure!(cache.is_dir(), "reconciliation cache missing");
        }
        let git = SharedGit::open(&cache, self.plan.plan().shared.clone())?;
        if !selected && prepared.is_none() {
            ensure!(
                git.refresh()?.commit == request.expected_remote_commit(),
                "remote differs from reconciliation request"
            );
        }
        git.verify_current_snapshot(request.expected_remote_commit())?;
        git.verify_ancestor(
            request.previous_remote_commit(),
            request.expected_remote_commit(),
        )?;
        let cache_identity = super::identity(&cache)?;
        let evidence = serde_json::to_string(
            &serde_json::json!({"version":1,"request_sha256":request.sha256(),
            "cache":cache,"cache_identity":cache_identity,"hosts_sha256":fence.hosts_sha256}),
        )?;
        self.store
            .retain_artifact(&prepared_name, &evidence, false)?;
        request
            .register(reverse, pool, &hosts, || {
                self.verify()?;
                ensure!(
                    super::identity(&cache)? == cache_identity
                        && self.store.read_artifact(&prepared_name)?.as_deref()
                            == Some(evidence.as_str())
                        && self
                            .store
                            .read_artifact(&format!("{prefix}-request.json"))?
                            .as_deref()
                            == Some(request.bytes()),
                    "reconciliation evidence changed"
                );
                for host in &fences {
                    ensure!(
                        self.store
                            .read_artifact(&format!(
                                "{prefix}-host-{}.json",
                                host.coordinator.context("coordinator missing")?.participant
                            ))?
                            .as_deref()
                            == Some(serde_json::to_string(host)?.as_str()),
                        "reconciliation host receipt changed"
                    );
                }
                git.verify_current_snapshot(request.expected_remote_commit())?;
                Ok(())
            })
            .await?;
        self.store
            .retain_artifact(&format!("{prefix}-selected.json"), request.bytes(), false)
            .context("reconciliation committed; retry exact request to retain local receipt")?;
        Ok(request.sha256().to_owned())
    }
}
