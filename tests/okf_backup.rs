#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::unix::fs::{PermissionsExt, symlink};
use ygg::knowledge::{
    document::Document,
    identity::IdentityRegistry,
    store::{ExpectedRevision, KnowledgeBackup, KnowledgeStore},
};

#[test]
fn backup_preserves_exact_bytes_unknown_files_empty_dirs_and_separate_identity() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = KnowledgeStore::open(&source, true).unwrap();
    let document = Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap();
    let written = store.put(&document, ExpectedRevision::Absent).unwrap();
    std::fs::create_dir(source.join("empty")).unwrap();
    std::fs::write(source.join("opaque"), [0, 255, 0, 3]).unwrap();
    std::fs::write(source.join(".lookup.json"), "discardable cache").unwrap();
    let target = temp.path().join("backup");
    let snapshot = store.backup(&target).unwrap();
    assert_eq!(snapshot, KnowledgeBackup::verify(&target).unwrap());
    assert!(!target.join("corpus/.writer.lock").exists());
    assert!(!target.join("corpus/.lookup.json").exists());
    assert!(target.join("corpus/empty").is_dir());
    assert_eq!(
        std::fs::read(target.join("corpus/opaque")).unwrap(),
        [0, 255, 0, 3]
    );
    assert_eq!(
        std::fs::read(source.join(written.key.relative_path())).unwrap(),
        std::fs::read(target.join("corpus").join(written.key.relative_path())).unwrap()
    );
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(target.join("corpus/opaque"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let recovered = temp.path().join("recovered");
    assert_eq!(
        KnowledgeBackup::restore(&target, &recovered).unwrap(),
        snapshot
    );
    assert_eq!(
        std::fs::read(recovered.join("opaque")).unwrap(),
        [0, 255, 0, 3]
    );
    assert!(recovered.join("empty").is_dir());
    assert_eq!(
        std::fs::read(recovered.join(written.key.relative_path())).unwrap(),
        std::fs::read(source.join(written.key.relative_path())).unwrap()
    );
    assert!(KnowledgeBackup::restore(&target, &recovered).is_err());
    assert!(KnowledgeBackup::restore(&target, &target.join("corpus/nested")).is_err());
    assert_eq!(KnowledgeBackup::verify(&target).unwrap(), snapshot);
    assert_eq!(
        KnowledgeBackup::verify_restored(&target, &recovered).unwrap(),
        snapshot
    );
    std::fs::write(recovered.join("opaque"), "independent edit").unwrap();
    assert!(KnowledgeBackup::verify_restored(&target, &recovered).is_err());

    let policy = temp.path().join("policy");
    let registry = IdentityRegistry::open(&policy, true).unwrap();
    let original = registry.initialize(true).unwrap();
    let saved_policy = temp.path().join("policy-backup");
    KnowledgeStore::open(&policy, false)
        .unwrap()
        .backup(&saved_policy)
        .unwrap();
    KnowledgeBackup::verify(&saved_policy).unwrap();
    let restored = IdentityRegistry::open(&saved_policy.join("corpus"), false)
        .unwrap()
        .read()
        .unwrap()
        .0;
    assert_eq!(original, restored);
}

#[test]
fn backup_verification_rejects_edits_extras_missing_files_and_manifest_changes() {
    let temp = tempfile::tempdir().unwrap();
    let store = KnowledgeStore::open(&temp.path().join("source"), true).unwrap();
    std::fs::write(temp.path().join("source/file"), "original").unwrap();
    for change in ["edit", "extra", "missing", "manifest", "symlink"] {
        let target = temp.path().join(change);
        store.backup(&target).unwrap();
        match change {
            "edit" => std::fs::write(target.join("corpus/file"), "modified").unwrap(),
            "extra" => std::fs::write(target.join("corpus/extra"), "extra").unwrap(),
            "missing" => std::fs::remove_file(target.join("corpus/file")).unwrap(),
            "manifest" => std::fs::write(target.join("knowledge-backup.json"), "{}").unwrap(),
            _ => {
                std::fs::remove_file(target.join("corpus/file")).unwrap();
                symlink(temp.path().join("source/file"), target.join("corpus/file")).unwrap();
            }
        }
        assert!(KnowledgeBackup::verify(&target).is_err(), "{change}");
    }
}

#[test]
fn backup_never_overwrites_and_refuses_nested_destinations_or_links() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = KnowledgeStore::open(&source, true).unwrap();
    std::fs::write(source.join("file"), "original").unwrap();
    let target = temp.path().join("backup");
    let original = store.backup(&target).unwrap();
    std::fs::write(source.join("file"), "changed").unwrap();
    assert!(store.backup(&target).is_err());
    assert_eq!(KnowledgeBackup::verify(&target).unwrap(), original);
    assert!(store.backup(&source.join("nested")).is_err());
    assert!(!source.join("nested").exists());
    symlink(&source, temp.path().join("alias")).unwrap();
    assert!(store.backup(&temp.path().join("alias/nested")).is_err());
    symlink(source.join("file"), source.join("link")).unwrap();
    assert!(store.backup(&temp.path().join("symlink-backup")).is_err());
    assert!(!temp.path().join("symlink-backup").exists());
    std::fs::remove_file(source.join("link")).unwrap();
    std::fs::hard_link(source.join("file"), source.join("link")).unwrap();
    assert!(store.backup(&temp.path().join("hardlink-backup")).is_err());
}

#[test]
fn backup_uses_open_source_descriptor_and_rejects_oversized_files() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = KnowledgeStore::open(&source, true).unwrap();
    std::fs::write(source.join("file"), "held source").unwrap();
    let moved = temp.path().join("moved");
    std::fs::rename(&source, &moved).unwrap();
    std::fs::create_dir(temp.path().join("outside")).unwrap();
    std::fs::write(temp.path().join("outside/file"), "outside source").unwrap();
    symlink(temp.path().join("outside"), &source).unwrap();
    let target = temp.path().join("backup");
    store.backup(&target).unwrap();
    assert_eq!(
        std::fs::read(target.join("corpus/file")).unwrap(),
        b"held source"
    );
    let file = std::fs::File::create(moved.join("huge")).unwrap();
    file.set_len(1024 * 1024 * 1024 + 1).unwrap();
    assert!(
        store
            .backup(&temp.path().join("oversized"))
            .unwrap_err()
            .to_string()
            .contains("size limit")
    );
    assert!(!temp.path().join("oversized").exists());
}

#[test]
fn paired_recovery_retains_both_writer_leases_and_detects_independent_edits() {
    let temp = tempfile::tempdir().unwrap();
    let corpus_path = temp.path().join("corpus");
    let policy_path = temp.path().join("policy");
    let corpus = KnowledgeStore::open(&corpus_path, true).unwrap();
    let policy = KnowledgeStore::open(&policy_path, true).unwrap();
    let document = Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap();
    corpus.put(&document, ExpectedRevision::Absent).unwrap();
    std::fs::write(policy_path.join("settings.json"), "{}").unwrap();
    assert!(
        corpus
            .backup_pair_retained(
                &policy,
                &policy_path.join("nested-backup"),
                &temp.path().join("unused-backup")
            )
            .is_err()
    );
    assert!(!policy_path.join("nested-backup").exists());
    assert!(!temp.path().join("unused-backup").exists());
    let corpus_archive = temp.path().join("corpus-archive");
    let policy_archive = temp.path().join("policy-archive");
    let saved = corpus
        .backup_pair_retained(&policy, &corpus_archive, &policy_archive)
        .unwrap();
    assert_eq!(
        saved.corpus(),
        &KnowledgeBackup::verify(&corpus_archive).unwrap()
    );
    assert_eq!(
        saved.policy(),
        &KnowledgeBackup::verify(&policy_archive).unwrap()
    );
    assert_eq!(saved.snapshot().unwrap().documents.len(), 1);
    let mut locks = Vec::new();
    for path in [&corpus_path, &policy_path] {
        for name in [".writer.lock", ".export.lock"] {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.join(name))
                .unwrap();
            assert!(fs2::FileExt::try_lock_exclusive(&file).is_err());
            locks.push(file);
        }
    }
    std::fs::write(policy_path.join("settings.json"), "{\"changed\":true}").unwrap();
    assert!(saved.verify_sources().is_err());
    assert!(saved.snapshot().is_err());
    // Retained recovery bytes survive an independent source edit unchanged.
    assert_eq!(
        saved.policy(),
        &KnowledgeBackup::verify(&policy_archive).unwrap()
    );
    drop(saved);
    for file in locks {
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
        fs2::FileExt::unlock(&file).unwrap();
    }
    assert!(
        corpus
            .backup_pair_retained(
                &corpus,
                &temp.path().join("same-a"),
                &temp.path().join("same-b")
            )
            .is_err()
    );
}
