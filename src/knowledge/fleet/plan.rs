//! Immutable request parsing. Declarations and digests are not authenticated
//! participant receipts; transport and live evidence checks precede execution.
use super::Registration;
use crate::knowledge::{
    document::digest, guard::CLIENT_PROTOCOL, identity::Identities, legacy::Mappings, shared,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

const MAX_PLAN_BYTES: usize = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    /// Absolute path on the machine owning this backup, never resolved remotely.
    pub path: PathBuf,
    pub manifest_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub id: Uuid,
    pub name: String,
    pub protocol: i32,
    pub corpus: PathBuf,
    pub policy: PathBuf,
    pub identities: Identities,
    pub backup: Backup,
    pub knowledge_writers_stopped: bool,
    pub external_editors_stopped: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub name: String,
    pub id: Uuid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub operation: Uuid,
    pub source_generation: i64,
    pub mappings: Mappings,
    pub agents: Vec<Agent>,
    pub shared: shared::Config,
    pub expected_remote_commit: String,
    pub source_backup: Backup,
    pub all_participating_hosts_listed: bool,
    pub schema_changes_stopped: bool,
    pub session_preserving_endpoint: bool,
    pub remote_writers_stopped: bool,
    pub participants: Vec<Participant>,
}

/// Owns the exact accepted bytes. Re-serialization must not silently change the
/// digest used by registration or later participant requests.
pub struct ValidatedPlan {
    plan: Plan,
    bytes: String,
    registration: Registration,
}
fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        && path
            .to_str()
            .is_some_and(|s| !s.chars().any(char::is_control))
}
fn backup(value: &Backup) -> Result<()> {
    ensure!(
        absolute(&value.path) && hex(&value.manifest_sha256, 64),
        "backup requires absolute host-local path and manifest SHA-256"
    );
    Ok(())
}
impl ValidatedPlan {
    pub fn parse(bytes: &str) -> Result<Self> {
        ensure!(bytes.len() <= MAX_PLAN_BYTES, "fleet plan exceeds 8 MiB");
        let plan: Plan = serde_json::from_str(bytes)?;
        let registration = Registration {
            version: plan.version,
            operation: plan.operation,
            request_sha256: digest(bytes.as_bytes()),
            database_id: plan.mappings.database_id,
            source_generation: plan.source_generation,
            corpus_id: plan.mappings.corpus_id,
            participants: plan.participants.iter().map(|p| p.id).collect(),
        };
        registration.expected()?;
        ensure!(
            plan.all_participating_hosts_listed
                && plan.schema_changes_stopped
                && plan.session_preserving_endpoint
                && plan.remote_writers_stopped,
            "complete census and schema/remote-writer quiescence declarations required"
        );
        ensure!(
            plan.mappings
                .repos
                .iter()
                .all(|(a, b)| !a.is_nil() && !b.is_nil())
                && plan.mappings.users.values().all(|u| !u.trim().is_empty()),
            "invalid explicit identity mapping"
        );
        let mut agents = BTreeSet::new();
        ensure!(
            plan.agents.iter().all(|agent| !agent.name.trim().is_empty()
                && !agent.id.is_nil()
                && agents.insert(&agent.name)),
            "duplicate or invalid agent mapping"
        );
        backup(&plan.source_backup)?;
        ensure!(
            hex(&plan.expected_remote_commit, 40) || hex(&plan.expected_remote_commit, 64),
            "expected remote commit required"
        );
        ensure!(
            plan.shared.version == 1
                && !plan.shared.remote.trim().is_empty()
                && !plan.shared.remote.starts_with('-')
                && !plan.shared.remote.chars().any(char::is_control),
            "invalid shared transport configuration"
        );
        ensure!(
            Command::new("git")
                .args([
                    "check-ref-format",
                    &format!("refs/heads/{}", plan.shared.branch)
                ])
                .status()?
                .success(),
            "invalid shared branch"
        );
        let mut names = BTreeSet::new();
        for host in &plan.participants {
            ensure!(
                !host.name.trim().is_empty()
                    && host.name.len() <= 128
                    && !host.name.chars().any(char::is_control)
                    && names.insert(&host.name),
                "duplicate or invalid participant name"
            );
            ensure!(
                host.protocol == CLIENT_PROTOCOL
                    && host.knowledge_writers_stopped
                    && host.external_editors_stopped,
                "compatible, quiesced participant required"
            );
            ensure!(
                absolute(&host.corpus)
                    && absolute(&host.policy)
                    && !host.corpus.starts_with(&host.policy)
                    && !host.policy.starts_with(&host.corpus),
                "participant corpus and policy require separate absolute host-local paths"
            );
            backup(&host.backup)?;
            ensure!(
                !host.backup.path.starts_with(&host.corpus)
                    && !host.corpus.starts_with(&host.backup.path)
                    && !host.backup.path.starts_with(&host.policy)
                    && !host.policy.starts_with(&host.backup.path),
                "participant backup overlaps corpus or policy"
            );
            host.identities.validate()?;
            ensure!(
                host.identities.trusted && host.identities.corpus_id == plan.mappings.corpus_id,
                "participant requires explicit trusted target corpus identity"
            );
            for (legacy, portable) in &plan.mappings.repos {
                ensure!(
                    host.identities.repos.iter().any(|r| r.id == *portable
                        && r.databases
                            .get(&plan.mappings.database_id)
                            .is_some_and(|ids| ids.contains(legacy))),
                    "participant lacks explicit legacy repository mapping"
                );
            }
        }
        Ok(Self {
            plan,
            bytes: bytes.to_owned(),
            registration,
        })
    }
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
    pub fn bytes(&self) -> &str {
        &self.bytes
    }
    pub fn registration(&self) -> &Registration {
        &self.registration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn fixture() -> Value {
        let corpus = Uuid::new_v4();
        json!({"version":1,"operation":Uuid::new_v4(),"source_generation":1,
            "mappings":{"database_id":Uuid::new_v4(),"corpus_id":corpus,"repos":{},"users":{"":"explicit-owner"}},
            "agents":[],
            "shared":{"version":1,"remote":"git@example.test:private/knowledge.git","branch":"main"},
            "expected_remote_commit":"a".repeat(40),
            "source_backup":{"path":"/coordinator/source-backup","manifest_sha256":"b".repeat(64)},
            "all_participating_hosts_listed":true,"schema_changes_stopped":true,
            "session_preserving_endpoint":true,"remote_writers_stopped":true,
            "participants":[{"id":Uuid::new_v4(),"name":"remote-host","protocol":CLIENT_PROTOCOL,
                "corpus":"/nonexistent/remote/corpus","policy":"/nonexistent/remote/policy",
                "identities":{"version":1,"corpus_id":corpus,"trusted":true,"repos":[]},
                "backup":{"path":"/nonexistent/remote/backup","manifest_sha256":"c".repeat(64)},
                "knowledge_writers_stopped":true,"external_editors_stopped":true}]})
    }
    #[test]
    fn exact_request_bytes_bind_registration_without_resolving_remote_paths() {
        let bytes = serde_json::to_string_pretty(&fixture()).unwrap();
        let saved = ValidatedPlan::parse(&bytes).unwrap();
        assert_eq!(saved.bytes(), bytes);
        assert_eq!(
            saved.registration().request_sha256,
            digest(bytes.as_bytes())
        );
        let compact = serde_json::to_string(saved.plan()).unwrap();
        assert_ne!(
            ValidatedPlan::parse(&compact)
                .unwrap()
                .registration()
                .request_sha256,
            saved.registration().request_sha256
        );
        assert_eq!(
            saved.registration().participants,
            vec![saved.plan().participants[0].id]
        );
    }
    #[test]
    fn preserves_distinct_host_paths_and_aliases_for_same_portable_repository() {
        let mut value = fixture();
        let legacy = Uuid::new_v4();
        let portable = Uuid::new_v4();
        let database = value["mappings"]["database_id"]
            .as_str()
            .unwrap()
            .to_owned();
        value["mappings"]["repos"] = json!({legacy.to_string():portable});
        value["participants"][0]["identities"]["repos"] = json!([{
            "id":portable,"aliases":["ssh://host-one/repo"],
            "common_dirs":["/host-one/repo/.git"], "databases":{database:[legacy]}}]);
        let mut second = value["participants"][0].clone();
        second["id"] = json!(Uuid::new_v4());
        second["name"] = json!("second-host");
        second["identities"]["repos"][0]["aliases"] = json!(["ssh://host-two/repo"]);
        second["identities"]["repos"][0]["common_dirs"] = json!(["/host-two/repo/.git"]);
        value["participants"].as_array_mut().unwrap().push(second);
        let saved = ValidatedPlan::parse(&value.to_string()).unwrap();
        let hosts = &saved.plan().participants;
        assert_ne!(
            hosts[0].identities.repos[0].aliases,
            hosts[1].identities.repos[0].aliases
        );
        assert_ne!(
            hosts[0].identities.repos[0].common_dirs,
            hosts[1].identities.repos[0].common_dirs
        );
        assert_eq!(
            hosts[0].identities.repos[0].id,
            hosts[1].identities.repos[0].id
        );
    }

    #[test]
    fn rejects_incomplete_census_duplicate_hosts_and_changed_identity() {
        for mutate in 0..10 {
            let mut value = fixture();
            match mutate {
                0 => value["all_participating_hosts_listed"] = json!(false),
                1 => {
                    let host = value["participants"][0].clone();
                    value["participants"].as_array_mut().unwrap().push(host);
                }
                2 => value["participants"][0]["identities"]["corpus_id"] = json!(Uuid::new_v4()),
                3 => {
                    value["participants"][0]["policy"] = json!("/nonexistent/remote/corpus/policy")
                }
                4 => value["participants"][0]["external_editors_stopped"] = json!(false),
                5 => value["source_backup"]["manifest_sha256"] = json!("A".repeat(64)),
                6 => value["shared"]["branch"] = json!("main..other"),
                7 => value["participants"][0]["protocol"] = json!(CLIENT_PROTOCOL + 1),
                8 => {
                    value["mappings"]["repos"] = json!({Uuid::new_v4().to_string(): Uuid::new_v4()})
                }
                _ => value["agents"] = json!([{"name":"author","id":Uuid::nil()}]),
            }
            assert!(
                ValidatedPlan::parse(&value.to_string()).is_err(),
                "case {mutate}"
            );
        }
    }
}
