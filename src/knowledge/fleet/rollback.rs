//! Fresh reverse-operation reservation for an activated fleet. Registration does
//! not fence hosts, certify remote freshness, import rows, or activate SQL.
use super::{Registration, plan::ValidatedPlan, transition};
use crate::knowledge::document::digest;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Participant {
    id: Uuid,
    knowledge_writers_stopped: bool,
    external_editors_stopped: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    operation: Uuid,
    forward_operation: Uuid,
    forward_request_sha256: String,
    activation_sha256: String,
    source_generation: i64,
    expected_remote_commit: String,
    all_participating_hosts_listed: bool,
    schema_changes_stopped: bool,
    session_preserving_endpoint: bool,
    remote_writers_stopped: bool,
    participants: Vec<Participant>,
}
/// Exact rollback request bytes are bound to the original validated fleet. Fresh
/// declarations are mandatory; forward-time quiescence is not reused as consent.
pub struct RollbackPlan {
    request: Request,
    bytes: String,
    sha256: String,
    forward: Registration,
}
fn hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl RollbackPlan {
    pub fn parse(forward: &ValidatedPlan, bytes: &str) -> Result<Self> {
        ensure!(bytes.len() <= 1024 * 1024, "rollback request exceeds 1 MiB");
        let request: Request = serde_json::from_str(bytes)?;
        let registration = forward.registration();
        ensure!(
            request.version == 1
                && !request.operation.is_nil()
                && request.operation != registration.operation
                && request.forward_operation == registration.operation
                && request.forward_request_sha256 == registration.request_sha256
                && request.source_generation == registration.source_generation + 2
                && request.source_generation < i64::MAX - 1,
            "rollback request differs from forward operation or active generation"
        );
        ensure!(
            hex(&request.activation_sha256, 64)
                && (hex(&request.expected_remote_commit, 40)
                    || hex(&request.expected_remote_commit, 64)),
            "rollback requires activation digest and exact remote commit"
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
            "rollback requires fresh complete-census and writer/schema quiescence declarations"
        );
        let participants: BTreeSet<_> = request.participants.iter().map(|h| h.id).collect();
        ensure!(
            participants.len() == request.participants.len()
                && participants == registration.participants.iter().copied().collect(),
            "rollback participant census differs from activated fleet"
        );
        Ok(Self {
            request,
            bytes: bytes.to_owned(),
            sha256: digest(bytes.as_bytes()),
            forward: registration.clone(),
        })
    }
    pub fn operation(&self) -> Uuid {
        self.request.operation
    }
    pub fn bytes(&self) -> &str {
        &self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn expected_remote_commit(&self) -> &str {
        &self.request.expected_remote_commit
    }

    /// Reserve under the exclusive database generation lease. Exact retries do
    /// not modify rows or generation; competing or changed requests cannot adopt
    /// a reservation. The supplied commit remains a declaration until capture.
    pub async fn register(&self, pool: &sqlx::PgPool) -> Result<()> {
        let mut tx = Registration::transaction(pool).await?;
        let activation = transition::saved_activation(&self.forward, &mut tx)
            .await?
            .context("rollback requires committed fleet activation")?;
        transition::verify_activation(&self.forward, &mut tx, &activation).await?;
        ensure!(
            activation.ready_sha256 == self.request.activation_sha256,
            "rollback activation digest differs from committed fleet"
        );
        let prior: Option<(Uuid, Uuid, i64, String, String)> = sqlx::query_as(
            "SELECT forward_operation_id,database_id,source_generation,request_sha256,request_json FROM public.knowledge_fleet_rollbacks WHERE operation_id=$1")
            .bind(self.operation()).fetch_optional(&mut *tx).await?;
        if let Some(prior) = prior {
            ensure!(
                prior
                    == (
                        self.forward.operation,
                        self.forward.database_id,
                        self.request.source_generation,
                        self.sha256.clone(),
                        self.bytes.clone()
                    ),
                "rollback reservation differs from exact request"
            );
        } else {
            let owner: Option<Uuid> = sqlx::query_scalar(
                "SELECT operation_id FROM public.knowledge_fleet_rollbacks WHERE database_id=$1 AND source_generation=$2")
                .bind(self.forward.database_id).bind(self.request.source_generation).fetch_optional(&mut *tx).await?;
            ensure!(
                owner.is_none(),
                "another rollback operation owns this active generation"
            );
            sqlx::query("INSERT INTO public.knowledge_fleet_rollbacks(operation_id,forward_operation_id,database_id,source_generation,request_sha256,request_json) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(self.operation()).bind(self.forward.operation).bind(self.forward.database_id)
                .bind(self.request.source_generation).bind(&self.sha256).bind(&self.bytes)
                .execute(&mut *tx).await?;
        }
        tx.commit()
            .await
            .context("rollback reservation outcome uncertain; retry exact request")
    }
}
