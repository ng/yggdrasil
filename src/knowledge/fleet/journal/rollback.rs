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
        if committed {
            // A later recovery archive may bind this cache. Reconcile without
            // fetching or rewriting its confirmed snapshot, even on failure.
            git.verify_current_snapshot(reverse.expected_remote_commit())?;
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
                git.verify_current_snapshot(reverse.expected_remote_commit())?;
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
