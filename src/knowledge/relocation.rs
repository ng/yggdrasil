//! A deployment restore may change the local bundle path, never its authority.
//! Derive the one permitted policy edit from the retained exact-byte archive.
use super::{
    document::digest,
    identity::IdentityRegistry,
    runtime::{Binding, Phase, SELECTION_FILE},
    store::{BackupEntry, KnowledgeBackup, KnowledgeStore},
};
use crate::db::backup::DatabaseSnapshot;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionRebase {
    pub source_bundle: PathBuf,
    pub restored_bundle: PathBuf,
    pub source_sha256: String,
    pub restored_sha256: String,
}

pub(crate) struct Prepared {
    pub evidence: SelectionRebase,
    original: String,
    restored: String,
}

pub(crate) fn prepare(
    archive: &Path,
    target_bundle: &Path,
    database: &DatabaseSnapshot,
) -> Result<Option<Prepared>> {
    ensure!(
        target_bundle.is_absolute(),
        "absolute relocated bundle path required"
    );
    let verified = KnowledgeBackup::verify(archive)?;
    let policy = KnowledgeStore::open(&archive.join("corpus"), false)?;
    let Some(original) = policy.read_control(SELECTION_FILE)? else {
        ensure!(
            database.backend != "okf",
            "OKF backup lacks local selection binding"
        );
        return Ok(None);
    };
    ensure!(
        verified.entries.get(SELECTION_FILE)
            == Some(&BackupEntry::File {
                bytes: original.len() as u64,
                sha256: digest(original.as_bytes()),
            }),
        "archived selection changed during relocation validation"
    );
    let mut binding: Binding = serde_json::from_str(&original)?;
    ensure!(
        binding.version == 1
            && binding.minimum_client > 0
            && binding.minimum_client <= super::guard::CLIENT_PROTOCOL
            && binding.generation > 0
            && binding.generation == database.generation
            && binding.mappings.database_id == database.database_id
            && Some(binding.mappings.corpus_id) == database.corpus_id
            && binding.bundle.is_absolute()
            && matches!(
                (&binding.phase, database.backend.as_str()),
                (Phase::Okf, "okf") | (Phase::Fenced, "fenced")
            ),
        "archived selection disagrees with database storage marker or client protocol"
    );
    let registry = IdentityRegistry::open(&archive.join("corpus"), false)?;
    ensure!(
        registry.read()?.0.corpus_id == binding.mappings.corpus_id,
        "archived selection corpus differs from policy"
    );
    for (legacy, portable) in &binding.mappings.repos {
        ensure!(
            registry.from_legacy(binding.mappings.database_id, *legacy)? == *portable,
            "archived selection repository mapping differs from policy"
        );
    }
    let source_bundle = binding.bundle.clone();
    binding.bundle = target_bundle.to_owned();
    let restored = serde_json::to_string_pretty(&binding)?;
    ensure!(
        KnowledgeBackup::verify(archive)? == verified,
        "policy archive changed during relocation validation"
    );
    Ok(Some(Prepared {
        evidence: SelectionRebase {
            source_bundle,
            restored_bundle: target_bundle.to_owned(),
            source_sha256: digest(original.as_bytes()),
            restored_sha256: digest(restored.as_bytes()),
        },
        original,
        restored,
    }))
}

impl Prepared {
    /// Called only on an unpublished restore stage, before configuration switch.
    pub(crate) fn apply(&self, policy: &Path) -> Result<()> {
        let policy = KnowledgeStore::open(policy, false)?;
        let _selection = policy.selection_lease(true)?;
        policy.update_control(SELECTION_FILE, |current| {
            ensure!(
                current == Some(self.original.as_str()),
                "restored selection changed before rebasing"
            );
            Ok((self.restored.clone(), ()))
        })
    }

    pub(crate) fn verify(&self, archive: &Path, policy: &Path) -> Result<()> {
        KnowledgeBackup::verify_restored_rebased_selection(archive, policy, &self.restored)
    }
}

pub(crate) fn verify_restored(
    archive: &Path,
    policy: &Path,
    bundle: &Path,
    database: &DatabaseSnapshot,
    evidence: Option<&SelectionRebase>,
) -> Result<()> {
    match (
        prepare(archive, &bundle.canonicalize()?, database)?,
        evidence,
    ) {
        (Some(prepared), Some(evidence)) => {
            ensure!(
                &prepared.evidence == evidence,
                "selection rebase receipt differs from archive/destination"
            );
            prepared.verify(archive, policy)
        }
        (None, None) => KnowledgeBackup::verify_restored(archive, policy).map(|_| ()),
        (Some(_), None) => {
            anyhow::bail!("selected corpus requires a restore with selection rebasing evidence")
        }
        (None, Some(_)) => anyhow::bail!("unexpected selection rebasing evidence"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::legacy::Mappings;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn fixture(root: &Path) -> DatabaseSnapshot {
        let policy = root.join("source-policy");
        let registry = IdentityRegistry::open(&policy, true).unwrap();
        let corpus = registry.initialize(true).unwrap().corpus_id;
        let database = DatabaseSnapshot {
            database_id: Uuid::new_v4(),
            generation: 7,
            backend: "okf".into(),
            corpus_id: Some(corpus),
            server_major: 16,
            tool_version: "fixture".into(),
            migrations: vec![],
            table_rows: BTreeMap::new(),
            bytes: 0,
            sha256: String::new(),
            validation: None,
        };
        let portable = registry
            .bind(&super::super::identity::GitIdentity {
                common_dir: root.join("checkout/.git"),
                origin: Some("https://example.com/portable-repo".into()),
            })
            .unwrap();
        let legacy = Uuid::new_v4();
        let (mut identities, revision) = registry.read().unwrap();
        identities
            .repos
            .iter_mut()
            .find(|repo| repo.id == portable)
            .unwrap()
            .databases
            .entry(database.database_id)
            .or_default()
            .insert(legacy);
        registry.replace(&revision, &identities).unwrap();
        let binding = Binding {
            version: 1,
            minimum_client: super::super::guard::CLIENT_PROTOCOL,
            generation: 7,
            phase: Phase::Okf,
            bundle: root.join("source-no-longer-present"),
            mappings: Mappings {
                database_id: database.database_id,
                corpus_id: corpus,
                repos: BTreeMap::from([(legacy, portable)]),
                users: BTreeMap::from([("old-user".into(), "portable-user".into())]),
            },
            agents: BTreeMap::from([("lead".into(), Uuid::new_v4())]),
        };
        let store = KnowledgeStore::open(&policy, false).unwrap();
        store
            .update_control(SELECTION_FILE, |_| {
                Ok((serde_json::to_string(&binding).unwrap(), ()))
            })
            .unwrap();
        store.backup(&root.join("archive")).unwrap();
        database
    }

    #[test]
    fn relocation_changes_only_path_and_keeps_archive_immutable() {
        let temp = tempfile::tempdir().unwrap();
        let database = fixture(temp.path());
        let archive = temp.path().join("archive");
        let before = KnowledgeBackup::verify(&archive).unwrap();
        let target = temp.path().canonicalize().unwrap().join("target-bundle");
        std::fs::create_dir(&target).unwrap();
        let prepared = prepare(&archive, &target, &database).unwrap().unwrap();
        let restored = temp.path().join("restored-policy");
        KnowledgeBackup::restore(&archive, &restored).unwrap();
        prepared.apply(&restored).unwrap();
        prepared.verify(&archive, &restored).unwrap();
        assert!(KnowledgeBackup::verify_restored(&archive, &restored).is_err());
        assert_eq!(KnowledgeBackup::verify(&archive).unwrap(), before);
        let mut original: serde_json::Value = serde_json::from_str(&prepared.original).unwrap();
        let actual: serde_json::Value = serde_json::from_str(&prepared.restored).unwrap();
        original["bundle"] = serde_json::to_value(&target).unwrap();
        assert_eq!(original, actual);
        verify_restored(
            &archive,
            &restored,
            &target,
            &database,
            Some(&prepared.evidence),
        )
        .unwrap();
        assert!(verify_restored(&archive, &restored, &target, &database, None).is_err());
        let mut forged = prepared.evidence.clone();
        forged.restored_sha256 = "forged".into();
        assert!(verify_restored(&archive, &restored, &target, &database, Some(&forged)).is_err());
        std::fs::write(restored.join("unexpected"), "must not be ignored").unwrap();
        assert!(prepared.verify(&archive, &restored).is_err());
    }

    #[test]
    fn mismatched_marker_or_edited_target_never_authorizes_rebase() {
        let temp = tempfile::tempdir().unwrap();
        let mut database = fixture(temp.path());
        let archive = temp.path().join("archive");
        let target = temp.path().join("target");
        database.generation += 1;
        assert!(prepare(&archive, &target, &database).is_err());
        database.generation -= 1;
        database.backend = "sql".into();
        assert!(prepare(&archive, &target, &database).is_err());
        database.backend = "okf".into();
        let prepared = prepare(&archive, &target, &database).unwrap().unwrap();
        let restored = temp.path().join("restored-policy");
        KnowledgeBackup::restore(&archive, &restored).unwrap();
        let policy = KnowledgeStore::open(&restored, false).unwrap();
        policy
            .update_control(SELECTION_FILE, |_| Ok(("{}".into(), ())))
            .unwrap();
        assert!(prepared.apply(&restored).is_err());
        assert_eq!(policy.read_control(SELECTION_FILE).unwrap().unwrap(), "{}");
    }

    #[test]
    fn relocating_a_fenced_selection_does_not_activate_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut database = fixture(temp.path());
        let policy = KnowledgeStore::open(&temp.path().join("source-policy"), false).unwrap();
        let mut binding: Binding =
            serde_json::from_str(&policy.read_control(SELECTION_FILE).unwrap().unwrap()).unwrap();
        binding.phase = Phase::Fenced;
        binding.generation += 1;
        database.backend = "fenced".into();
        database.generation += 1;
        policy
            .update_control(SELECTION_FILE, |_| {
                Ok((serde_json::to_string(&binding).unwrap(), ()))
            })
            .unwrap();
        let archive = temp.path().join("fenced-archive");
        policy.backup(&archive).unwrap();
        let prepared = prepare(&archive, &temp.path().join("target"), &database)
            .unwrap()
            .unwrap();
        let moved: Binding = serde_json::from_str(&prepared.restored).unwrap();
        assert!(matches!(moved.phase, Phase::Fenced));
        assert_eq!(moved.generation, database.generation);
        database.backend = "okf".into();
        assert!(prepare(&archive, &temp.path().join("target"), &database).is_err());
    }

    #[test]
    fn missing_selection_is_allowed_only_without_active_okf_marker() {
        let temp = tempfile::tempdir().unwrap();
        let mut database = fixture(temp.path());
        let policy = temp.path().join("empty-policy");
        IdentityRegistry::open(&policy, true)
            .unwrap()
            .initialize(true)
            .unwrap();
        KnowledgeStore::open(&policy, false)
            .unwrap()
            .backup(&temp.path().join("empty-archive"))
            .unwrap();
        assert!(
            prepare(
                &temp.path().join("empty-archive"),
                &temp.path().join("target"),
                &database
            )
            .is_err()
        );
        database.backend = "sql".into();
        database.corpus_id = None;
        assert!(
            prepare(
                &temp.path().join("empty-archive"),
                &temp.path().join("target"),
                &database
            )
            .unwrap()
            .is_none()
        );
    }
}
