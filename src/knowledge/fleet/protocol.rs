//! Typed preparation/cancellation exchange. A matching authenticated receipt is
//! evidence of that host operation, not permission to activate the fleet.
use super::{journal::Journal, plan::ValidatedPlan, transport};
use crate::knowledge::fence::SqlPreparation;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

const MAX_REQUEST: usize = 8 * 1024 * 1024;
const MAX_RESPONSE: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    PrepareSql,
    CancelSql,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    operation: Uuid,
    participant: Uuid,
    request_sha256: String,
    nonce: Uuid,
    action: Action,
    /// Exact original plan bytes, never a reconstructed JSON object.
    plan: String,
}
pub struct Request {
    envelope: Envelope,
    plan: ValidatedPlan,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    operation: Uuid,
    participant: Uuid,
    request_sha256: String,
    nonce: Uuid,
    action: Action,
    preparation: SqlPreparation,
}
/// This type is constructed only from the matching successful SSH exchange.
/// It deliberately has no public deserializer or file-import constructor.
pub struct AuthenticatedPreparation {
    preparation: SqlPreparation,
    action: Action,
}
impl AuthenticatedPreparation {
    pub fn preparation(&self) -> &SqlPreparation {
        &self.preparation
    }
    pub fn action(&self) -> Action {
        self.action
    }
}
impl Request {
    pub fn new(plan: &ValidatedPlan, participant: Uuid, action: Action) -> Result<Self> {
        let bytes = serde_json::to_vec(&Envelope {
            version: 1,
            operation: plan.plan().operation,
            participant,
            request_sha256: plan.registration().request_sha256.clone(),
            nonce: Uuid::new_v4(),
            action,
            plan: plan.bytes().to_owned(),
        })?;
        Self::parse(&bytes)
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_REQUEST,
            "participant request exceeds 8 MiB"
        );
        let envelope: Envelope = serde_json::from_slice(bytes)?;
        let plan = ValidatedPlan::parse(&envelope.plan)?;
        ensure!(
            envelope.version == 1
                && !envelope.nonce.is_nil()
                && envelope.operation == plan.plan().operation
                && envelope.request_sha256 == plan.registration().request_sha256
                && plan
                    .plan()
                    .participants
                    .iter()
                    .any(|p| p.id == envelope.participant),
            "participant request differs from complete plan"
        );
        Ok(Self { envelope, plan })
    }
    pub fn plan(&self) -> &ValidatedPlan {
        &self.plan
    }
    pub fn participant(&self) -> Uuid {
        self.envelope.participant
    }
    pub fn action(&self) -> Action {
        self.envelope.action
    }
    pub fn bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&self.envelope)?)
    }
    /// Participant-side serialization after executing the requested host action.
    /// This does not construct authenticated coordinator-side evidence.
    pub fn respond(&self, preparation: SqlPreparation) -> Result<Vec<u8>> {
        self.validate_preparation(&preparation)?;
        let result = serde_json::to_vec(&Response {
            version: 1,
            operation: self.envelope.operation,
            participant: self.participant(),
            request_sha256: self.envelope.request_sha256.clone(),
            nonce: self.envelope.nonce,
            action: self.action(),
            preparation,
        })?;
        ensure!(
            result.len() <= MAX_RESPONSE,
            "participant response exceeds 1 MiB"
        );
        Ok(result)
    }
    /// Host-side handler for an authenticated operator invocation. Uses the
    /// host's actual deployment configuration, never coordinator-local paths.
    /// This library handler is not an enabled CLI migration entry point.
    pub async fn execute(
        &self,
        config: &crate::config::database::DeploymentConfig,
        pool: &sqlx::PgPool,
    ) -> Result<Vec<u8>> {
        use crate::knowledge::{
            fence,
            guard::CLIENT_PROTOCOL,
            runtime::{Binding, Phase},
        };
        let plan = self.plan.plan();
        let host = plan
            .participants
            .iter()
            .find(|p| p.id == self.participant())
            .unwrap();
        ensure!(
            config.knowledge_dir.canonicalize()? == host.corpus
                && config.knowledge_policy_dir.canonicalize()? == host.policy,
            "participant request differs from actual host configuration"
        );
        let binding = Binding {
            version: 1,
            minimum_client: CLIENT_PROTOCOL,
            generation: plan.source_generation,
            phase: Phase::Fenced,
            bundle: host.corpus.clone(),
            mappings: serde_json::from_value(serde_json::to_value(&plan.mappings)?)?,
            agents: plan.agents.iter().map(|a| (a.name.clone(), a.id)).collect(),
        };
        let coordinator = fence::CoordinatorBinding {
            migration_operation: plan.operation,
            participant: host.id,
        };
        let result = match self.action() {
            Action::PrepareSql => {
                fence::prepare_sql_backed(
                    config,
                    &binding,
                    coordinator,
                    &self.envelope.request_sha256,
                    &host.backup.path,
                    &host.backup.manifest_sha256,
                    pool,
                )
                .await?
            }
            Action::CancelSql => {
                fence::cancel_sql_backed(
                    config,
                    &binding,
                    coordinator,
                    &self.envelope.request_sha256,
                    &host.backup.path,
                    &host.backup.manifest_sha256,
                    pool,
                )
                .await?
            }
        };
        self.respond(result)
    }

    fn validate_preparation(&self, result: &SqlPreparation) -> Result<()> {
        let host = self
            .plan
            .plan()
            .participants
            .iter()
            .find(|p| p.id == self.participant())
            .unwrap();
        let plan = self.plan.plan();
        ensure!(
            result.coordinator.migration_operation == plan.operation
                && result.coordinator.participant == host.id
                && result.database_id == plan.mappings.database_id
                && result.corpus_id == plan.mappings.corpus_id
                && result.source_generation == plan.source_generation
                && result.policy == host.policy,
            "participant preparation differs from planned host/source"
        );
        for value in [&result.intent_sha256, &result.fenced_sha256] {
            ensure!(
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "participant evidence requires SHA-256 digests"
            );
        }
        Ok(())
    }
    fn accept(&self, bytes: &[u8]) -> Result<AuthenticatedPreparation> {
        ensure!(
            bytes.len() <= MAX_RESPONSE,
            "participant response exceeds 1 MiB"
        );
        let response: Response = serde_json::from_slice(bytes)?;
        ensure!(
            response.version == 1
                && response.operation == self.envelope.operation
                && response.participant == self.participant()
                && response.request_sha256 == self.envelope.request_sha256
                && response.nonce == self.envelope.nonce
                && response.action == self.action(),
            "stale or mismatched participant response"
        );
        self.validate_preparation(&response.preparation)?;
        Ok(AuthenticatedPreparation {
            preparation: response.preparation,
            action: response.action,
        })
    }
}
/// The journal is revalidated both before dispatch and after response receipt.
/// Failure never removes a fence or authorizes a database transition.
pub async fn call(
    journal: &Journal,
    participant: Uuid,
    action: Action,
    identity: Option<&Path>,
) -> Result<AuthenticatedPreparation> {
    let plan = journal.plan()?;
    let request = Request::new(plan, participant, action)?;
    let host = plan
        .plan()
        .participants
        .iter()
        .find(|p| p.id == participant)
        .context("participant missing")?;
    let response =
        transport::exchange(&host.endpoint, participant, &request.bytes()?, identity).await?;
    journal.plan()?;
    request.accept(&response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::fence::CoordinatorBinding;
    use serde_json::{Value, json};
    fn request() -> Request {
        let plan = ValidatedPlan::parse(&super::super::plan::tests::fixture().to_string()).unwrap();
        Request::new(&plan, plan.plan().participants[0].id, Action::PrepareSql).unwrap()
    }
    fn receipt(request: &Request) -> SqlPreparation {
        let p = request.plan.plan();
        SqlPreparation {
            coordinator: CoordinatorBinding {
                migration_operation: p.operation,
                participant: request.participant(),
            },
            source_generation: p.source_generation,
            database_id: p.mappings.database_id,
            corpus_id: p.mappings.corpus_id,
            policy: p.participants[0].policy.clone(),
            intent_sha256: "a".repeat(64),
            fenced_sha256: "b".repeat(64),
        }
    }
    #[test]
    fn rejects_replay_and_all_cross_request_response_fields() {
        let request = request();
        let bytes = request.respond(receipt(&request)).unwrap();
        assert_eq!(request.accept(&bytes).unwrap().action(), Action::PrepareSql);
        let retry = Request::new(&request.plan, request.participant(), request.action()).unwrap();
        assert!(retry.accept(&bytes).is_err());
        let source: Value = serde_json::from_slice(&bytes).unwrap();
        for field in [
            "version",
            "operation",
            "participant",
            "request_sha256",
            "nonce",
            "action",
        ] {
            let mut changed = source.clone();
            changed[field] = match field {
                "version" => json!(2),
                "request_sha256" => json!("0".repeat(64)),
                "action" => json!("cancel_sql"),
                _ => json!(Uuid::new_v4()),
            };
            assert!(
                request
                    .accept(&serde_json::to_vec(&changed).unwrap())
                    .is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn rejects_wrong_host_evidence_and_tampered_plan_request() {
        let request = request();
        let mut wrong = receipt(&request);
        wrong.policy = "/another/host/policy".into();
        assert!(request.respond(wrong).is_err());
        let mut wrong = receipt(&request);
        wrong.source_generation += 1;
        assert!(request.respond(wrong).is_err());
        let mut wrong = receipt(&request);
        wrong.intent_sha256 = "invalid".into();
        assert!(request.respond(wrong).is_err());
        let mut value: Value = serde_json::from_slice(&request.bytes().unwrap()).unwrap();
        value["plan"] = json!(format!("{} ", request.plan.bytes()));
        assert!(Request::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(Request::new(&request.plan, Uuid::new_v4(), Action::PrepareSql).is_err());
    }
}
