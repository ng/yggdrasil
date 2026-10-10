use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCancellation {
    pub coordinator: CoordinatorBinding,
    pub request_sha256: String,
    pub database_id: Uuid,
    pub corpus_id: Uuid,
    pub source_generation: i64,
    pub policy: PathBuf,
    pub original_sha256: String,
    pub fence: Option<LocalFence>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelIntent {
    version: u32,
    result: LocalCancellation,
    policy_identity: (u64, u64),
    bundle_identity: (u64, u64),
    original: String,
    fence_intent: Option<String>,
}
impl FenceLease<'_> {
    fn cancellation_fence(
        &self,
        bytes: &str,
        generation: i64,
        coordinator: CoordinatorBinding,
        original: &str,
    ) -> Result<LocalFence> {
        let intent: Intent = serde_json::from_str(bytes)?;
        let binding = intent.validate(self.config, generation)?;
        ensure!(
            intent.coordinator == Some(coordinator) && intent.original == original,
            "cancellation cannot adopt another local fence"
        );
        Ok(LocalFence {
            operation: intent.operation,
            coordinator: intent.coordinator,
            database_id: binding.mappings.database_id,
            corpus_id: binding.mappings.corpus_id,
            source_generation: generation,
            policy: self.path.clone(),
            original_sha256: digest(intent.original.as_bytes()),
            fenced_sha256: digest(intent.fenced.as_bytes()),
        })
    }
    /// Caller holds a shared SQL generation lease and has verified the immutable
    /// cancellation barrier. This restores only the exact original selection.
    pub(crate) fn cancel(
        &self,
        generation: i64,
        coordinator: CoordinatorBinding,
        expected: &Binding,
        request_sha256: &str,
        known_fence: Option<&LocalFence>,
    ) -> Result<LocalCancellation> {
        self.policy.verify_root_path(&self.path)?;
        let original = serde_json::to_string(expected)?;
        ensure!(
            expected.phase == Phase::Okf
                && expected.generation == generation
                && self.config.knowledge_dir.canonicalize()? == expected.bundle,
            "cancellation binding differs from configured corpus"
        );
        let fence_name = format!("local-fence-{generation}.json");
        let intent_name = format!(
            "local-rollback-cancel-{}-intent.json",
            coordinator.migration_operation
        );
        let done_name = format!(
            "local-rollback-cancel-{}-done.json",
            coordinator.migration_operation
        );
        let live = self.policy.read_artifact(&fence_name)?;
        let saved = self.policy.read_artifact(&intent_name)?;
        let intent: CancelIntent = if let Some(saved) = &saved {
            serde_json::from_str(saved)?
        } else {
            ensure!(
                self.policy.read_artifact(&done_name)?.is_none(),
                "cancellation intent missing behind completion"
            );
            let fence = live
                .as_deref()
                .map(|b| self.cancellation_fence(b, generation, coordinator, &original))
                .transpose()?;
            ensure!(
                known_fence.is_none_or(|known| Some(known) == fence.as_ref()),
                "known host fence missing before cancellation"
            );
            if fence.is_none() {
                ensure!(
                    self.policy.read_control(SELECTION_FILE)?.as_deref() == Some(original.as_str()),
                    "unfenced host does not retain original selection"
                );
            }
            CancelIntent {
                version: 1,
                result: LocalCancellation {
                    coordinator,
                    request_sha256: request_sha256.to_owned(),
                    database_id: expected.mappings.database_id,
                    corpus_id: expected.mappings.corpus_id,
                    source_generation: generation,
                    policy: self.path.clone(),
                    original_sha256: digest(original.as_bytes()),
                    fence,
                },
                policy_identity: identity(&self.path)?,
                bundle_identity: identity(&expected.bundle)?,
                original: original.clone(),
                fence_intent: live.clone(),
            }
        };
        ensure!(
            intent.version == 1
                && self.config.knowledge_policy_dir.canonicalize()? == self.path
                && intent.original == original
                && intent.policy_identity == identity(&self.path)?
                && intent.bundle_identity == identity(&expected.bundle)?,
            "saved cancellation roots or original binding changed"
        );
        let archived = intent
            .fence_intent
            .as_deref()
            .map(|b| self.cancellation_fence(b, generation, coordinator, &original))
            .transpose()?;
        let result = LocalCancellation {
            coordinator,
            request_sha256: request_sha256.to_owned(),
            database_id: expected.mappings.database_id,
            corpus_id: expected.mappings.corpus_id,
            source_generation: generation,
            policy: self.path.clone(),
            original_sha256: digest(original.as_bytes()),
            fence: archived,
        };
        ensure!(
            intent.result == result
                && known_fence.is_none_or(|known| Some(known) == result.fence.as_ref()),
            "saved cancellation differs from requested host evidence"
        );
        ensure!(
            live.is_none() || live == intent.fence_intent,
            "live fence belongs to another operation"
        );
        let current = self.policy.read_control(SELECTION_FILE)?;
        let fenced = intent
            .fence_intent
            .as_deref()
            .map(serde_json::from_str::<Intent>)
            .transpose()?
            .map(|i| i.fenced);
        ensure!(
            current.as_deref() == Some(original.as_str())
                || (live.is_some() && fenced.is_some() && current == fenced),
            "selection changed or fence evidence missing before restoration"
        );
        let bytes = serde_json::to_string(&intent)?;
        let completion = serde_json::to_string(&result)?;
        ensure!(
            self.policy
                .read_artifact(&done_name)?
                .as_ref()
                .is_none_or(|b| b == &completion),
            "cancellation completion changed"
        );
        self.policy.retain_artifact(&intent_name, &bytes, false)?;
        if current.as_deref() != Some(original.as_str()) {
            self.policy.update_control(SELECTION_FILE, |actual| {
                ensure!(
                    actual == current.as_deref(),
                    "selection changed during cancellation"
                );
                Ok((original.clone(), ()))
            })?;
        }
        if let Some(live) = &live {
            self.policy.remove_control(&fence_name, live)?;
        }
        self.policy.verify_root_path(&self.path)?;
        ensure!(
            self.config.knowledge_policy_dir.canonicalize()? == self.path
                && self.config.knowledge_dir.canonicalize()? == expected.bundle
                && identity(&expected.bundle)? == intent.bundle_identity
                && self.policy.read_control(SELECTION_FILE)?.as_deref() == Some(original.as_str())
                && self.policy.read_artifact(&fence_name)?.is_none()
                && self.policy.read_artifact(&intent_name)?.as_deref() == Some(bytes.as_str()),
            "host cancellation changed during restoration"
        );
        self.policy
            .retain_artifact(&done_name, &completion, false)?;
        Ok(result)
    }
}
