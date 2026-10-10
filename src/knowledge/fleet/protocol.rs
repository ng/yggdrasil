//! Typed preparation, inspection and recovery exchange. A matching authenticated receipt is
//! evidence of that host operation, not permission to activate the fleet.
use super::{journal::Journal, plan::ValidatedPlan, transport};
use crate::knowledge::fence::SqlPreparation;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

// Version 9 additionally checks cancellation before every rollback host fence.
// Older hosts must fail before the coordinator fences SQL.
const RPC_VERSION: u32 = 9;
mod rollback;
pub use rollback::{
    AuthenticatedDeselection, AuthenticatedRollbackFence, call_rollback_deselect,
    call_rollback_fence,
};
// Preserve the forward limit while reserving room for an escaped 1 MiB
// reverse request, so a previously accepted fleet remains addressable.
pub const MAX_REQUEST: usize = 11 * 1024 * 1024;
const MAX_FORWARD_REQUEST: usize = 8 * 1024 * 1024;
const MAX_RESPONSE: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    PrepareSql,
    CancelSql,
    InspectSql,
    AbortSql,
    ReadySql,
    FinalizeSql,
    FenceOkf,
    DeselectOkf,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activation_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollback_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sql_return: Option<super::journal::SqlReturnReceipt>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selection_sha256: Option<String>,
}
/// This type is constructed only from the matching successful SSH exchange.
/// It deliberately has no public deserializer or file-import constructor.
pub struct AuthenticatedPreparation {
    preparation: SqlPreparation,
    action: Action,
    readiness: Option<crate::knowledge::fence::SqlReadiness>,
    selection_sha256: Option<String>,
}
impl AuthenticatedPreparation {
    pub fn preparation(&self) -> &SqlPreparation {
        &self.preparation
    }
    pub fn action(&self) -> Action {
        self.action
    }
    pub fn selection_sha256(&self) -> Option<&str> {
        self.selection_sha256.as_deref()
    }
    pub fn readiness(&self) -> Option<&crate::knowledge::fence::SqlReadiness> {
        self.readiness.as_ref()
    }
}
impl Request {
    pub fn new(plan: &ValidatedPlan, participant: Uuid, action: Action) -> Result<Self> {
        Self::build(plan, participant, action, None, None)
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
            None,
        )
    }
    pub fn new_finalize(
        plan: &ValidatedPlan,
        participant: Uuid,
        publication: &super::journal::Publication,
        activation_sha256: &str,
    ) -> Result<Self> {
        Self::build(
            plan,
            participant,
            Action::FinalizeSql,
            Some(publication.clone()),
            Some(activation_sha256.to_owned()),
        )
    }
    fn build(
        plan: &ValidatedPlan,
        participant: Uuid,
        action: Action,
        publication: Option<super::journal::Publication>,
        activation_sha256: Option<String>,
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
            activation_sha256,
            rollback_request: None,
            sql_return: None,
        })?;
        Self::parse(&bytes)
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_REQUEST,
            "participant request exceeds 11 MiB"
        );
        let envelope: Envelope = serde_json::from_slice(bytes)?;
        ensure!(
            matches!(envelope.action, Action::FenceOkf | Action::DeselectOkf)
                || bytes.len() <= MAX_FORWARD_REQUEST,
            "forward participant request exceeds 8 MiB"
        );
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
            matches!(envelope.action, Action::ReadySql | Action::FinalizeSql)
                == envelope.publication.is_some(),
            "readiness/finalization require publication; other actions cannot carry it"
        );
        ensure!(
            (envelope.action == Action::FinalizeSql) == envelope.activation_sha256.is_some(),
            "finalization requires activation digest only"
        );
        ensure!(
            matches!(envelope.action, Action::FenceOkf | Action::DeselectOkf)
                == envelope.rollback_request.is_some(),
            "rollback fence requires a dedicated rollback request only"
        );
        ensure!(
            (envelope.action == Action::DeselectOkf) == envelope.sql_return.is_some(),
            "deselection requires SQL return receipt only"
        );
        if let Some(reverse) = &envelope.rollback_request {
            let reverse = super::rollback::RollbackPlan::parse(&plan, reverse)?;
            if let Some(receipt) = &envelope.sql_return {
                reverse.validate_return(&plan, receipt)?;
            }
        }
        if let Some(hash) = &envelope.activation_sha256 {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid activation digest"
            );
        }
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
        self.respond_with(preparation, None, None)
    }
    fn respond_ready(&self, ready: crate::knowledge::fence::SqlReadiness) -> Result<Vec<u8>> {
        self.respond_with(ready.preparation.clone(), Some(ready), None)
    }
    fn respond_finalized(
        &self,
        result: crate::knowledge::fence::SqlFinalization,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.envelope.activation_sha256.as_deref() == Some(result.activation_sha256.as_str()),
            "finalization activation differs"
        );
        self.respond_with(
            result.readiness.preparation.clone(),
            Some(result.readiness),
            Some(result.selected_sha256),
        )
    }
    fn respond_with(
        &self,
        preparation: SqlPreparation,
        readiness: Option<crate::knowledge::fence::SqlReadiness>,
        selection_sha256: Option<String>,
    ) -> Result<Vec<u8>> {
        self.validate_preparation(&preparation)?;
        self.validate_readiness(readiness.as_ref(), &preparation)?;
        self.validate_selection(selection_sha256.as_deref())?;
        let result = serde_json::to_vec(&Response {
            version: RPC_VERSION,
            operation: self.envelope.operation,
            participant: self.participant(),
            request_sha256: self.envelope.request_sha256.clone(),
            nonce: self.envelope.nonce,
            action: self.action(),
            preparation,
            readiness,
            selection_sha256,
        })?;
        ensure!(
            result.len()
                <= if self.action() == Action::ReadySql {
                    MAX_RESPONSE - 256
                } else {
                    MAX_RESPONSE
                },
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
            (if self.action() == Action::FinalizeSql {
                config.knowledge_dir == host.corpus
            } else {
                config.knowledge_dir.canonicalize()? == host.corpus
            }) && config.knowledge_policy_dir.canonicalize()? == host.policy,
            "participant request differs from actual host configuration"
        );
        if self.action() == Action::PrepareSql {
            ensure!(
                config.knowledge_dir == host.corpus,
                "fleet preparation requires canonical configured corpus path"
            );
        }
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
            Action::DeselectOkf => {
                return self.respond_rollback(
                    self.rollback()?
                        .deselect_host(
                            &self.plan,
                            config,
                            self.participant(),
                            self.envelope.sql_return.as_ref().unwrap(),
                            pool,
                        )
                        .await?,
                );
            }
            Action::FenceOkf => {
                return self.respond_rollback(
                    self.rollback()?
                        .fence_host(&self.plan, config, self.participant(), pool)
                        .await?,
                );
            }
            Action::FinalizeSql => {
                return self.respond_finalized(
                    fence::finalize_sql_backed(
                        config,
                        &self.plan,
                        self.participant(),
                        self.envelope.activation_sha256.as_ref().unwrap(),
                        pool,
                    )
                    .await?,
                );
            }
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
        if !matches!(self.action(), Action::ReadySql | Action::FinalizeSql) {
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
        for hash in [
            &ready.archive_revision,
            &ready.intent_sha256,
            &ready.swap_sha256,
        ] {
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
    fn validate_selection(&self, selected: Option<&str>) -> Result<()> {
        if self.action() != Action::FinalizeSql {
            ensure!(selected.is_none(), "unexpected selection receipt");
            return Ok(());
        }
        let p = self.plan.plan();
        let host = p
            .participants
            .iter()
            .find(|h| h.id == self.participant())
            .unwrap();
        let binding = crate::knowledge::runtime::Binding {
            version: 1,
            minimum_client: crate::knowledge::guard::CLIENT_PROTOCOL,
            generation: p.source_generation + 2,
            phase: crate::knowledge::runtime::Phase::Okf,
            bundle: host.corpus.clone(),
            mappings: serde_json::from_value(serde_json::to_value(&p.mappings)?)?,
            agents: p.agents.iter().map(|a| (a.name.clone(), a.id)).collect(),
        };
        let expected =
            crate::knowledge::document::digest(serde_json::to_string(&binding)?.as_bytes());
        ensure!(
            selected == Some(expected.as_str()),
            "selected binding differs from requested activation"
        );
        Ok(())
    }
    fn accept(&self, bytes: &[u8]) -> Result<AuthenticatedPreparation> {
        ensure!(
            bytes.len()
                <= if self.action() == Action::ReadySql {
                    MAX_RESPONSE - 256
                } else {
                    MAX_RESPONSE
                },
            "participant response exceeds phase limit"
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
        self.validate_selection(response.selection_sha256.as_deref())?;
        Ok(AuthenticatedPreparation {
            preparation: response.preparation,
            action: response.action,
            readiness: response.readiness,
            selection_sha256: response.selection_sha256,
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
pub async fn call_finalize(
    journal: &Journal,
    participant: Uuid,
    publication: &super::journal::Publication,
    activation_sha256: &str,
    identity: Option<&Path>,
) -> Result<AuthenticatedPreparation> {
    let request =
        Request::new_finalize(journal.plan()?, participant, publication, activation_sha256)?;
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
        for version in [1, 2, 3, 4, 5, 6, 7, 8] {
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
            swap_sha256: "f".repeat(64),
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
            "/readiness/swap_sha256",
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
        let final_request = Request::new_finalize(
            &plain.plan,
            plain.participant(),
            &publication,
            &"f".repeat(64),
        )
        .unwrap();
        assert!(
            Request::new_finalize(&plain.plan, plain.participant(), &publication, "invalid")
                .is_err()
        );
        let ready: crate::knowledge::fence::SqlReadiness =
            serde_json::from_value(original["readiness"].clone()).unwrap();
        assert!(final_request.respond_ready(ready).is_err());
        let mut wrong_selection = original.clone();
        wrong_selection["action"] = json!(Action::FinalizeSql);
        wrong_selection["nonce"] = json!(final_request.envelope.nonce);
        wrong_selection["selection_sha256"] = json!("0".repeat(64));
        assert!(
            final_request
                .accept(&serde_json::to_vec(&wrong_selection).unwrap())
                .is_err()
        );
        let mut incomplete: Value =
            serde_json::from_slice(&final_request.bytes().unwrap()).unwrap();
        incomplete
            .as_object_mut()
            .unwrap()
            .remove("activation_sha256");
        assert!(Request::parse(&serde_json::to_vec(&incomplete).unwrap()).is_err());
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
