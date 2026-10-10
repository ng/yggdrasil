//! Typed preparation, inspection and recovery exchange. A matching authenticated receipt is
//! evidence of that host operation, not permission to activate the fleet.
use super::{journal::Journal, plan::ValidatedPlan, transport};
use crate::knowledge::fence::SqlPreparation;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

// Version 3 additionally requires publication-bound host readiness.
// Older hosts must fail before the coordinator fences SQL.
const RPC_VERSION: u32 = 3;
const MAX_REQUEST: usize = 8 * 1024 * 1024;
const MAX_RESPONSE: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    PrepareSql,
    CancelSql,
    InspectSql,
    AbortSql,
    ReadySql,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication: Option<super::journal::Publication>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    readiness: Option<crate::knowledge::fence::SqlReadiness>,
}
/// This type is constructed only from the matching successful SSH exchange.
/// It deliberately has no public deserializer or file-import constructor.
pub struct AuthenticatedPreparation {
    preparation: SqlPreparation,
    action: Action,
    readiness: Option<crate::knowledge::fence::SqlReadiness>,
}
impl AuthenticatedPreparation {
    pub fn preparation(&self) -> &SqlPreparation {
        &self.preparation
    }
    pub fn action(&self) -> Action {
        self.action
    }
    pub fn readiness(&self) -> Option<&crate::knowledge::fence::SqlReadiness> {
        self.readiness.as_ref()
    }
}
impl Request {
    pub fn new(plan: &ValidatedPlan, participant: Uuid, action: Action) -> Result<Self> {
        Self::build(plan, participant, action, None)
    }
    pub fn new_ready(
        plan: &ValidatedPlan,
        participant: Uuid,
        publication: &super::journal::Publication,
    ) -> Result<Self> {
        Self::build(
            plan,
            participant,
            Action::ReadySql,
            Some(publication.clone()),
        )
    }
    fn build(
        plan: &ValidatedPlan,
        participant: Uuid,
        action: Action,
        publication: Option<super::journal::Publication>,
    ) -> Result<Self> {
        let bytes = serde_json::to_vec(&Envelope {
            version: RPC_VERSION,
            operation: plan.plan().operation,
            participant,
            request_sha256: plan.registration().request_sha256.clone(),
            nonce: Uuid::new_v4(),
            action,
            plan: plan.bytes().to_owned(),
            publication,
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
            envelope.version == RPC_VERSION
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
        ensure!(
            (envelope.action == Action::ReadySql) == envelope.publication.is_some(),
            "readiness requires an explicit publication; other actions cannot carry it"
        );
        if let Some(publication) = &envelope.publication {
            publication.validate_plan(&plan)?;
        }
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
        self.respond_with(preparation, None)
    }
    fn respond_ready(&self, ready: crate::knowledge::fence::SqlReadiness) -> Result<Vec<u8>> {
        self.respond_with(ready.preparation.clone(), Some(ready))
    }
    fn respond_with(
        &self,
        preparation: SqlPreparation,
        readiness: Option<crate::knowledge::fence::SqlReadiness>,
    ) -> Result<Vec<u8>> {
        self.validate_preparation(&preparation)?;
        self.validate_readiness(readiness.as_ref(), &preparation)?;
        let result = serde_json::to_vec(&Response {
            version: RPC_VERSION,
            operation: self.envelope.operation,
            participant: self.participant(),
            request_sha256: self.envelope.request_sha256.clone(),
            nonce: self.envelope.nonce,
            action: self.action(),
            preparation,
            readiness,
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
            Action::ReadySql => {
                return self.respond_ready(
                    fence::ready_sql_backed(
                        config,
                        &self.plan,
                        self.participant(),
                        self.envelope.publication.as_ref().unwrap(),
                        pool,
                    )
                    .await?,
                );
            }
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
            Action::InspectSql => {
                fence::inspect_sql_backed(
                    config,
                    &binding,
                    coordinator,
                    &self.envelope.request_sha256,
                    &host.backup.path,
                    &host.backup.manifest_sha256,
                    &plan.source_backup.manifest_sha256,
                    pool,
                )
                .await?
            }
            Action::AbortSql => {
                fence::abort_sql_backed(
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
    fn validate_readiness(
        &self,
        ready: Option<&crate::knowledge::fence::SqlReadiness>,
        preparation: &SqlPreparation,
    ) -> Result<()> {
        if self.action() != Action::ReadySql {
            ensure!(ready.is_none(), "unexpected readiness receipt");
            return Ok(());
        }
        let ready = ready.context("readiness receipt missing")?;
        let publication = self
            .envelope
            .publication
            .as_ref()
            .context("publication missing")?;
        let host = self
            .plan
            .plan()
            .participants
            .iter()
            .find(|h| h.id == self.participant())
            .unwrap();
        let staging = host
            .corpus
            .parent()
            .context("corpus parent missing")?
            .join(format!(
                ".ygg-fleet-{}-{}",
                self.plan.plan().operation,
                host.id
            ));
        ensure!(
            &ready.preparation == preparation
                && &ready.publication == publication
                && ready.staging == staging
                && ready.staging_identity.1 > 0
                && ready.candidate_identity.1 > 0,
            "readiness differs from requested host or publication"
        );
        for hash in [&ready.archive_revision, &ready.intent_sha256] {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "readiness requires retained SHA-256 evidence"
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
            response.version == RPC_VERSION
                && response.operation == self.envelope.operation
                && response.participant == self.participant()
                && response.request_sha256 == self.envelope.request_sha256
                && response.nonce == self.envelope.nonce
                && response.action == self.action(),
            "stale or mismatched participant response"
        );
        self.validate_preparation(&response.preparation)?;
        self.validate_readiness(response.readiness.as_ref(), &response.preparation)?;
        Ok(AuthenticatedPreparation {
            preparation: response.preparation,
            action: response.action,
            readiness: response.readiness,
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
    exchange_request(journal, request, identity).await
}
pub async fn call_ready(
    journal: &Journal,
    participant: Uuid,
    publication: &super::journal::Publication,
    identity: Option<&Path>,
) -> Result<AuthenticatedPreparation> {
    let request = Request::new_ready(journal.plan()?, participant, publication)?;
    exchange_request(journal, request, identity).await
}
async fn exchange_request(
    journal: &Journal,
    request: Request,
    identity: Option<&Path>,
) -> Result<AuthenticatedPreparation> {
    let participant = request.participant();
    let host = journal
        .plan()?
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
                "version" => json!(1),
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
    fn rejects_preparation_only_protocol_before_execution() {
        let request = request();
        let mut legacy: Value = serde_json::from_slice(&request.bytes().unwrap()).unwrap();
        for version in [1, 2] {
            legacy["version"] = json!(version);
            assert!(Request::parse(&serde_json::to_vec(&legacy).unwrap()).is_err());
        }
    }
    #[test]
    fn readiness_is_bound_to_publication_preparation_and_nonce() {
        let plain = request();
        let p = plain.plan.plan();
        let publication = serde_json::from_value(json!({"version":1,
            "operation":p.operation,"request_sha256":plain.plan.registration().request_sha256,
            "base":p.expected_remote_commit,"manifest_sha256":"a".repeat(64),
            "desired_sha256":"b".repeat(64),"commit":"c".repeat(40)}))
        .unwrap();
        let request = Request::new_ready(&plain.plan, plain.participant(), &publication).unwrap();
        let ready = crate::knowledge::fence::SqlReadiness {
            preparation: receipt(&request),
            publication: publication.clone(),
            staging: p.participants[0].corpus.parent().unwrap().join(format!(
                ".ygg-fleet-{}-{}",
                p.operation,
                plain.participant()
            )),
            staging_identity: (1, 1),
            candidate_identity: (1, 2),
            archive_revision: "d".repeat(64),
            intent_sha256: "e".repeat(64),
        };
        assert!(request.respond(ready.preparation.clone()).is_err());
        let bytes = request.respond_ready(ready).unwrap();
        assert!(request.accept(&bytes).unwrap().readiness().is_some());
        let retry = Request::new_ready(&plain.plan, plain.participant(), &publication).unwrap();
        assert!(retry.accept(&bytes).is_err());
        let original: Value = serde_json::from_slice(&bytes).unwrap();
        for pointer in [
            "/readiness/publication/commit",
            "/readiness/staging",
            "/readiness/preparation/intent_sha256",
            "/readiness/archive_revision",
        ] {
            let mut altered = original.clone();
            *altered.pointer_mut(pointer).unwrap() = json!("changed");
            assert!(
                request
                    .accept(&serde_json::to_vec(&altered).unwrap())
                    .is_err()
            );
        }
        let mut missing: Value = serde_json::from_slice(&request.bytes().unwrap()).unwrap();
        missing.as_object_mut().unwrap().remove("publication");
        assert!(Request::parse(&serde_json::to_vec(&missing).unwrap()).is_err());
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
