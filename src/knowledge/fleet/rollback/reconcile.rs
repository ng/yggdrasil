//! Explicit, append-only replacement of a stale rollback snapshot.
use super::*;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    operation: Uuid,
    rollback_operation: Uuid,
    rollback_request_sha256: String,
    fenced_generation: i64,
    previous_request_sha256: String,
    previous_remote_commit: String,
    expected_remote_commit: String,
    all_participating_hosts_listed: bool,
    schema_changes_stopped: bool,
    session_preserving_endpoint: bool,
    remote_writers_stopped: bool,
    participants: Vec<Participant>,
}

pub struct ReconciliationPlan {
    request: Request,
    bytes: String,
    sha256: String,
}
impl ReconciliationPlan {
    pub fn parse(reverse: &RollbackPlan, bytes: &str) -> Result<Self> {
        ensure!(
            bytes.len() <= 1024 * 1024,
            "reconciliation request exceeds 1 MiB"
        );
        let request: Request = serde_json::from_str(bytes)?;
        ensure!(
            request.version == 1
                && !request.operation.is_nil()
                && request.operation != reverse.operation()
                && request.operation != reverse.forward.operation
                && request.rollback_operation == reverse.operation()
                && request.rollback_request_sha256 == reverse.sha256()
                && request.fenced_generation == reverse.request.source_generation + 1,
            "reconciliation differs from rollback request"
        );
        ensure!(
            hex(&request.previous_request_sha256, 64)
                && [
                    request.previous_remote_commit.as_str(),
                    request.expected_remote_commit.as_str()
                ]
                .iter()
                .all(|c| hex(c, 40) || hex(c, 64))
                && request.previous_remote_commit != request.expected_remote_commit,
            "reconciliation requires predecessor digest and a different exact commit"
        );
        ensure!(
            request.all_participating_hosts_listed
                && request.schema_changes_stopped
                && request.session_preserving_endpoint
                && request.remote_writers_stopped
                && request
                    .participants
                    .iter()
                    .all(|h| h.knowledge_writers_stopped && h.external_editors_stopped),
            "reconciliation requires fresh writer and schema quiescence declarations"
        );
        let hosts: BTreeSet<_> = request.participants.iter().map(|h| h.id).collect();
        ensure!(
            hosts.len() == request.participants.len()
                && hosts == reverse.forward.participants.iter().copied().collect(),
            "reconciliation participant census differs"
        );
        Ok(Self {
            request,
            bytes: bytes.to_owned(),
            sha256: digest(bytes.as_bytes()),
        })
    }
    pub fn operation(&self) -> Uuid {
        self.request.operation
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn bytes(&self) -> &str {
        &self.bytes
    }
    pub fn expected_remote_commit(&self) -> &str {
        &self.request.expected_remote_commit
    }
    pub fn previous_remote_commit(&self) -> &str {
        &self.request.previous_remote_commit
    }
    pub(in crate::knowledge::fleet) fn prefix(&self, reverse: &RollbackPlan) -> String {
        format!(
            "rollback-{}-reconcile-{}",
            reverse.operation(),
            self.operation()
        )
    }
    pub(in crate::knowledge::fleet) async fn register(
        &self,
        reverse: &RollbackPlan,
        pool: &sqlx::PgPool,
        hosts: &str,
        verify: impl Fn() -> Result<()>,
    ) -> Result<()> {
        // Parse against the supplied operation again: callers cannot combine a
        // validated request with a different reverse operation.
        Self::parse(reverse, &self.bytes)?;
        let mut tx = reverse
            .fenced_transaction(pool, &digest(hosts.as_bytes()))
            .await?;
        let current = reverse.reconciliation_on(&mut tx).await?;
        verify()?;
        if let Some(current) = &current {
            if current.sha256 == self.sha256 {
                ensure!(current.bytes == self.bytes, "reconciliation bytes changed");
                tx.commit().await?;
                return Ok(());
            }
        }
        let previous_sha = current.as_ref().map_or(reverse.sha256(), |c| c.sha256());
        let previous_commit = current
            .as_ref()
            .map_or(reverse.expected_remote_commit(), |c| {
                c.expected_remote_commit()
            });
        ensure!(
            self.request.previous_request_sha256 == previous_sha
                && self.previous_remote_commit() == previous_commit,
            "reconciliation predecessor is no longer current"
        );
        sqlx::query("INSERT INTO public.knowledge_fleet_rollback_reconciliations(operation_id,rollback_operation_id,previous_request_sha256,previous_remote_commit,remote_commit,request_sha256,request_json,hosts_sha256) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(self.operation()).bind(reverse.operation()).bind(&self.request.previous_request_sha256)
            .bind(self.previous_remote_commit()).bind(self.expected_remote_commit()).bind(self.sha256()).bind(self.bytes())
            .bind(digest(hosts.as_bytes())).execute(&mut *tx).await?;
        verify()?;
        tx.commit()
            .await
            .context("reconciliation outcome uncertain; resume exact request")
    }
}
impl RollbackPlan {
    pub(in crate::knowledge::fleet) async fn reconciliation_on(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<Option<ReconciliationPlan>> {
        let row: Option<(String,String)> = sqlx::query_as("SELECT request_sha256,request_json FROM public.knowledge_fleet_rollback_reconciliations r WHERE rollback_operation_id=$1 AND NOT EXISTS (SELECT 1 FROM public.knowledge_fleet_rollback_reconciliations n WHERE n.rollback_operation_id=r.rollback_operation_id AND n.previous_request_sha256=r.request_sha256)")
            .bind(self.operation()).fetch_optional(&mut **tx).await?;
        row.map(|(sha, bytes)| {
            let result = ReconciliationPlan::parse(self, &bytes)?;
            ensure!(result.sha256 == sha, "reconciliation digest differs");
            Ok(result)
        })
        .transpose()
    }
    pub(in crate::knowledge::fleet) async fn reconciliation(
        &self,
        pool: &sqlx::PgPool,
    ) -> Result<Option<ReconciliationPlan>> {
        let mut tx = Registration::transaction(pool).await?;
        let result = self.reconciliation_on(&mut tx).await?;
        if result.is_some() {
            self.registered_on(&mut tx).await?;
        }
        // Absence conveys no authority: a first capture still registers and
        // fences through the normal workflow before reading or importing data.
        tx.commit().await?;
        Ok(result)
    }
}
