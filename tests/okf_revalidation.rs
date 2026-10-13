#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
use ygg::knowledge::{
    document::Document,
    store::{ExpectedRevision, KnowledgeStore},
};

#[test]
fn repeated_reads_observe_same_length_edits_even_with_restored_mtime() {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let mut document = Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap();
    document
        .activate(
            uuid::Uuid::new_v4(),
            ygg::knowledge::document::ActivationKind::Reviewed,
            None,
            Some(chrono::Utc::now()),
        )
        .unwrap();
    let original = store.put(&document, ExpectedRevision::Absent).unwrap();
    let path = temp.path().join(original.key.relative_path());
    assert_eq!(store.get(original.key).unwrap().unwrap().document, document);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut edited = document.clone();
    edited.body = "x".repeat(document.body.len());
    let text = edited.serialize().unwrap();
    assert_eq!(text.len(), fs::read(&path).unwrap().len());
    fs::write(&path, &text).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let current = store.get(original.key).unwrap().unwrap();
    assert_ne!(current.revision, original.revision);
    assert_eq!(current.document, edited);
    let corpus = document
        .profile()
        .unwrap()
        .unwrap()
        .approval
        .unwrap()
        .corpus_id;
    assert!(!current.document.activation_valid(corpus).unwrap());
    fs::remove_file(path).unwrap();
    assert!(store.get(original.key).unwrap().is_none());
    assert!(store.snapshot().documents.is_empty());
}

#[test]
fn repeated_reads_cannot_mask_corruption_symlinks_or_duplicate_ids() {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let document = Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap();
    let original = store.put(&document, ExpectedRevision::Absent).unwrap();
    let path = temp.path().join(original.key.relative_path());
    assert_eq!(store.snapshot().documents.len(), 1);
    fs::write(&path, "bad yaml").unwrap();
    let snapshot = store.snapshot();
    assert!(snapshot.documents.is_empty());
    assert!(!snapshot.diagnostics.is_empty());
    fs::write(&path, document.serialize().unwrap()).unwrap();
    assert_eq!(store.snapshot().documents.len(), 1);
    let duplicate = temp.path().join("global/notes");
    fs::create_dir_all(&duplicate).unwrap();
    fs::copy(&path, duplicate.join(format!("{}.md", original.key.id))).unwrap();
    assert!(store.snapshot().documents.is_empty());
    assert!(store.find(original.key.id).is_err());
    fs::remove_dir_all(duplicate).unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), document.serialize().unwrap()).unwrap();
    fs::remove_file(&path).unwrap();
    symlink(outside.path(), &path).unwrap();
    assert!(store.get(original.key).is_err());
    assert!(store.snapshot().documents.is_empty());
}

#[test]
fn batch_revalidation_rejects_duplicates_and_retains_unaffected_documents() {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let mut doc = Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap();
    let first = store.put(&doc, ExpectedRevision::Absent).unwrap();
    let mut p = doc.profile().unwrap().unwrap();
    p.id = uuid::Uuid::new_v4();
    doc.set_profile(&p).unwrap();
    let second = store.put(&doc, ExpectedRevision::Absent).unwrap();
    let selected = vec![first.clone(), second.clone()];
    assert_eq!(store.revalidate_selected(&selected).documents.len(), 2);
    let duplicate = temp.path().join("global/notes");
    fs::create_dir_all(&duplicate).unwrap();
    fs::copy(
        temp.path().join(first.key.relative_path()),
        duplicate.join(format!("{}.md", first.key.id)),
    )
    .unwrap();
    let current = store.revalidate_selected(&selected);
    assert_eq!(current.documents.len(), 1);
    assert_eq!(current.documents[0].key, second.key);
    assert!(!current.diagnostics.is_empty());
}
