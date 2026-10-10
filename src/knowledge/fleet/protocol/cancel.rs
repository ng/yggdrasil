use super::*;
use crate::knowledge::{
    fence::{LocalCancellation, LocalFence},
    fleet::rollback::{CancellationReceipt, RollbackPlan},
};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CancellationRequest {
    pub receipt: CancellationReceipt,
    pub known_fence: Option<LocalFence>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelResponse {
    version: u32,
    operation: Uuid,
    participant: Uuid,
    request_sha256: String,
    rollback_sha256: String,
    nonce: Uuid,
    action: Action,
    cancellation: CancellationRequest,
    result: LocalCancellation,
}
pub struct AuthenticatedCancellation(LocalCancellation);
impl AuthenticatedCancellation {
    pub fn result(&self) -> &LocalCancellation {
        &self.0
    }
}
impl Request {
    pub fn new_rollback_cancel(
        forward: &ValidatedPlan,
        participant: Uuid,
        reverse: &RollbackPlan,
        receipt: &CancellationReceipt,
        known_fence: Option<&LocalFence>,
    ) -> Result<Self> {
        let mut request = Self::new_rollback(forward, participant, reverse)?;
        request.envelope.action = Action::CancelRollback;
        request.envelope.cancellation = Some(CancellationRequest {
            receipt: receipt.clone(),
            known_fence: known_fence.cloned(),
        });
        Self::parse(&serde_json::to_vec(&request.envelope)?)
    }
    fn validate_cancelled(&self, result: &LocalCancellation) -> Result<()> {
        ensure!(
            self.action() == Action::CancelRollback,
            "not a rollback cancellation request"
        );
        let reverse = self.rollback()?;
        let request = self
            .envelope
            .cancellation
            .as_ref()
            .context("cancellation request missing")?;
        let expected = reverse.expected_binding(&self.plan, self.participant())?;
        let host = self
            .plan
            .plan()
            .participants
            .iter()
            .find(|h| h.id == self.participant())
            .unwrap();
        ensure!(
            result.coordinator
                == crate::knowledge::fence::CoordinatorBinding {
                    migration_operation: reverse.operation(),
                    participant: self.participant()
                }
                && result.request_sha256 == reverse.sha256()
                && result.database_id == expected.mappings.database_id
                && result.corpus_id == expected.mappings.corpus_id
                && result.source_generation == expected.generation
                && result.policy == host.policy
                && result.original_sha256
                    == crate::knowledge::document::digest(
                        serde_json::to_string(&expected)?.as_bytes()
                    ),
            "restored host differs from cancellation request"
        );
        if let Some(fence) = &result.fence {
            self.validate_fence(&reverse, fence)?;
        }
        ensure!(
            request
                .known_fence
                .as_ref()
                .is_none_or(|known| result.fence.as_ref() == Some(known)),
            "restored host lost known fence evidence"
        );
        Ok(())
    }
    pub(super) fn respond_cancelled(&self, result: LocalCancellation) -> Result<Vec<u8>> {
        self.validate_cancelled(&result)?;
        let response = serde_json::to_vec(&CancelResponse {
            version: RPC_VERSION,
            operation: self.envelope.operation,
            participant: self.participant(),
            request_sha256: self.envelope.request_sha256.clone(),
            rollback_sha256: self.rollback()?.sha256().to_owned(),
            nonce: self.envelope.nonce,
            action: self.action(),
            cancellation: self.envelope.cancellation.clone().unwrap(),
            result,
        })?;
        ensure!(
            response.len() <= MAX_RESPONSE,
            "cancellation response exceeds limit"
        );
        Ok(response)
    }
    pub(super) fn accept_cancelled(&self, bytes: &[u8]) -> Result<AuthenticatedCancellation> {
        ensure!(
            bytes.len() <= MAX_RESPONSE,
            "cancellation response exceeds limit"
        );
        let response: CancelResponse = serde_json::from_slice(bytes)?;
        ensure!(
            response.version == RPC_VERSION
                && response.operation == self.envelope.operation
                && response.participant == self.participant()
                && response.request_sha256 == self.envelope.request_sha256
                && response.rollback_sha256 == self.rollback()?.sha256()
                && response.nonce == self.envelope.nonce
                && response.action == self.action()
                && Some(&response.cancellation) == self.envelope.cancellation.as_ref(),
            "cancellation response differs from authenticated request"
        );
        self.validate_cancelled(&response.result)?;
        Ok(AuthenticatedCancellation(response.result))
    }
}
pub async fn call_rollback_cancel(
    journal: &Journal,
    reverse: &RollbackPlan,
    participant: Uuid,
    receipt: &CancellationReceipt,
    known_fence: Option<&LocalFence>,
    identity: Option<&Path>,
) -> Result<AuthenticatedCancellation> {
    let request =
        Request::new_rollback_cancel(journal.plan()?, participant, reverse, receipt, known_fence)?;
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
    request.accept_cancelled(&response)
}
