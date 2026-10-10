use super::*;
use crate::knowledge::{fence::LocalFence, fleet::rollback::RollbackPlan};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReverseResponse {
    version: u32,
    operation: Uuid,
    participant: Uuid,
    request_sha256: String,
    rollback_sha256: String,
    nonce: Uuid,
    action: Action,
    fence: LocalFence,
}
/// Constructed only from a matching, pinned-SSH exchange.
pub struct AuthenticatedRollbackFence(LocalFence);
impl AuthenticatedRollbackFence {
    pub fn fence(&self) -> &LocalFence {
        &self.0
    }
}
impl Request {
    pub fn new_rollback(
        forward: &ValidatedPlan,
        participant: Uuid,
        rollback: &RollbackPlan,
    ) -> Result<Self> {
        Self::parse(&serde_json::to_vec(&Envelope {
            version: RPC_VERSION,
            operation: forward.plan().operation,
            participant,
            request_sha256: forward.registration().request_sha256.clone(),
            nonce: Uuid::new_v4(),
            action: Action::FenceOkf,
            plan: forward.bytes().to_owned(),
            publication: None,
            activation_sha256: None,
            rollback_request: Some(rollback.bytes().to_owned()),
        })?)
    }
    pub(super) fn rollback(&self) -> Result<RollbackPlan> {
        ensure!(
            self.action() == Action::FenceOkf,
            "not a rollback fence request"
        );
        RollbackPlan::parse(
            &self.plan,
            self.envelope
                .rollback_request
                .as_deref()
                .context("rollback request missing")?,
        )
    }
    fn validate_fence(&self, rollback: &RollbackPlan, fence: &LocalFence) -> Result<()> {
        let mut binding = rollback.expected_binding(&self.plan, self.participant())?;
        let original =
            crate::knowledge::document::digest(serde_json::to_string(&binding)?.as_bytes());
        binding.phase = crate::knowledge::runtime::Phase::Fenced;
        let expected =
            crate::knowledge::document::digest(serde_json::to_string(&binding)?.as_bytes());
        let host = self
            .plan
            .plan()
            .participants
            .iter()
            .find(|h| h.id == self.participant())
            .unwrap();
        ensure!(
            !fence.operation.is_nil()
                && fence.coordinator
                    == Some(crate::knowledge::fence::CoordinatorBinding {
                        migration_operation: rollback.operation(),
                        participant: self.participant()
                    })
                && fence.database_id == binding.mappings.database_id
                && fence.corpus_id == binding.mappings.corpus_id
                && fence.source_generation == binding.generation
                && fence.policy == host.policy
                && fence.original_sha256 == original
                && fence.fenced_sha256 == expected,
            "rollback fence differs from requested participant binding"
        );
        Ok(())
    }
    pub(super) fn respond_rollback(&self, fence: LocalFence) -> Result<Vec<u8>> {
        let rollback = self.rollback()?;
        self.validate_fence(&rollback, &fence)?;
        let result = serde_json::to_vec(&ReverseResponse {
            version: RPC_VERSION,
            operation: self.envelope.operation,
            participant: self.participant(),
            request_sha256: self.envelope.request_sha256.clone(),
            rollback_sha256: rollback.sha256().to_owned(),
            nonce: self.envelope.nonce,
            action: self.action(),
            fence,
        })?;
        ensure!(
            result.len() <= MAX_RESPONSE,
            "rollback response exceeds limit"
        );
        Ok(result)
    }
    fn accept_rollback(&self, bytes: &[u8]) -> Result<AuthenticatedRollbackFence> {
        ensure!(
            bytes.len() <= MAX_RESPONSE,
            "rollback response exceeds limit"
        );
        let rollback = self.rollback()?;
        let response: ReverseResponse = serde_json::from_slice(bytes)?;
        ensure!(
            response.version == RPC_VERSION
                && response.operation == self.envelope.operation
                && response.participant == self.participant()
                && response.request_sha256 == self.envelope.request_sha256
                && response.rollback_sha256 == rollback.sha256()
                && response.nonce == self.envelope.nonce
                && response.action == self.action(),
            "rollback response differs from authenticated request"
        );
        self.validate_fence(&rollback, &response.fence)?;
        Ok(AuthenticatedRollbackFence(response.fence))
    }
}
pub async fn call_rollback_fence(
    journal: &Journal,
    rollback: &RollbackPlan,
    participant: Uuid,
    identity: Option<&Path>,
) -> Result<AuthenticatedRollbackFence> {
    let request = Request::new_rollback(journal.plan()?, participant, rollback)?;
    let host = journal
        .plan()?
        .plan()
        .participants
        .iter()
        .find(|h| h.id == participant)
        .context("participant missing")?;
    let response =
        transport::exchange(&host.endpoint, participant, &request.bytes()?, identity).await?;
    journal.plan()?;
    request.accept_rollback(&response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    #[test]
    fn rollback_fence_binds_nonce_request_and_complete_selected_binding() {
        let plan =
            ValidatedPlan::parse(&crate::knowledge::fleet::plan::tests::fixture().to_string())
                .unwrap();
        let participant = plan.plan().participants[0].id;
        let rollback = RollbackPlan::parse(&plan, &json!({
            "version":1,"operation":Uuid::new_v4(),"forward_operation":plan.plan().operation,
            "forward_request_sha256":plan.registration().request_sha256,"activation_sha256":"a".repeat(64),
            "source_generation":plan.plan().source_generation+2,"expected_remote_commit":"b".repeat(40),
            "all_participating_hosts_listed":true,"schema_changes_stopped":true,"session_preserving_endpoint":true,"remote_writers_stopped":true,
            "participants":plan.plan().participants.iter().map(|h|json!({"id":h.id,"knowledge_writers_stopped":true,"external_editors_stopped":true})).collect::<Vec<_>>()
        }).to_string()).unwrap();
        let request = Request::new_rollback(&plan, participant, &rollback).unwrap();
        let mut binding = rollback.expected_binding(&plan, participant).unwrap();
        let original_sha256 =
            crate::knowledge::document::digest(serde_json::to_string(&binding).unwrap().as_bytes());
        binding.phase = crate::knowledge::runtime::Phase::Fenced;
        let fence = LocalFence {
            operation: Uuid::new_v4(),
            coordinator: Some(crate::knowledge::fence::CoordinatorBinding {
                migration_operation: rollback.operation(),
                participant,
            }),
            database_id: binding.mappings.database_id,
            corpus_id: binding.mappings.corpus_id,
            source_generation: binding.generation,
            policy: plan.plan().participants[0].policy.clone(),
            original_sha256,
            fenced_sha256: crate::knowledge::document::digest(
                serde_json::to_string(&binding).unwrap().as_bytes(),
            ),
        };
        let response = request.respond_rollback(fence.clone()).unwrap();
        assert_eq!(request.accept_rollback(&response).unwrap().fence(), &fence);
        assert!(
            Request::new_rollback(&plan, participant, &rollback)
                .unwrap()
                .accept_rollback(&response)
                .is_err()
        );
        let original: Value = serde_json::from_slice(&response).unwrap();
        for (field, value) in [
            ("operation", json!(Uuid::new_v4())),
            ("participant", json!(Uuid::new_v4())),
            ("rollback_sha256", json!("0".repeat(64))),
            ("request_sha256", json!("0".repeat(64))),
            ("action", json!(Action::FinalizeSql)),
        ] {
            let mut altered = original.clone();
            altered[field] = value;
            assert!(
                request
                    .accept_rollback(&serde_json::to_vec(&altered).unwrap())
                    .is_err()
            );
        }
        for (field, value) in [
            ("source_generation", json!(1)),
            ("original_sha256", json!("0".repeat(64))),
            ("fenced_sha256", json!("0".repeat(64))),
            ("policy", json!("/wrong")),
            ("operation", json!(Uuid::nil())),
        ] {
            let mut altered = original.clone();
            altered["fence"][field] = value;
            assert!(
                request
                    .accept_rollback(&serde_json::to_vec(&altered).unwrap())
                    .is_err()
            );
        }
        let mut missing: Value = serde_json::from_slice(&request.bytes().unwrap()).unwrap();
        missing.as_object_mut().unwrap().remove("rollback_request");
        assert!(Request::parse(&serde_json::to_vec(&missing).unwrap()).is_err());
        assert!(Request::new(&plan, participant, Action::FenceOkf).is_err());
        // A forward request near its old limit must still carry the bounded
        // reverse request later; otherwise cutover could strand a large fleet.
        let mut large = plan.bytes().to_owned();
        large.extend(std::iter::repeat_n(
            ' ',
            MAX_FORWARD_REQUEST - 4096 - large.len(),
        ));
        let large = ValidatedPlan::parse(&large).unwrap();
        Request::new(&large, participant, Action::PrepareSql).unwrap();
        let mut reverse: Value = serde_json::from_str(rollback.bytes()).unwrap();
        reverse["forward_request_sha256"] = json!(large.registration().request_sha256);
        let mut reverse = reverse.to_string();
        reverse.extend(std::iter::repeat_n(' ', 1024 * 1024 - reverse.len()));
        let reverse = RollbackPlan::parse(&large, &reverse).unwrap();
        let request = Request::new_rollback(&large, participant, &reverse).unwrap();
        assert!(request.bytes().unwrap().len() > MAX_FORWARD_REQUEST);
        Request::parse(&request.bytes().unwrap()).unwrap();
    }
}
