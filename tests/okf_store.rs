#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Barrier},
};
use uuid::Uuid;
use ygg::knowledge::{
    document::{ActivationKind, Document},
    store::{ExpectedRevision, Key, KnowledgeStore},
};

fn private_temp() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn doc() -> Document {
    Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap()
}

#[test]
fn conditional_writes_deletion_and_reopen_preserve_durable_content() {
    let temp = private_temp();
    let root = temp.path().join("knowledge");
    assert!(KnowledgeStore::open(&root, false).is_err());
    assert!(!root.exists());
    let store = KnowledgeStore::open(&root, true).unwrap();
    let first = store.put(&doc(), ExpectedRevision::Absent).unwrap();
    assert!(store.put(&doc(), ExpectedRevision::Absent).is_err());
    let mut updated = first.document.clone();
    updated.body.push_str("Another rule.\n");
    let second = store
        .put(&updated, ExpectedRevision::Digest(&first.revision))
        .unwrap();
    assert!(
        store
            .put(&first.document, ExpectedRevision::Digest(&first.revision))
            .is_err()
    );
    assert!(store.delete(first.key, &first.revision).is_err());
    drop(store);
    let reopened = KnowledgeStore::open(&root, false).unwrap();
    let read = reopened.get(first.key).unwrap().unwrap();
    assert_eq!(read.document, updated);
    assert_eq!(read.revision, second.revision);
    reopened.delete(read.key, &read.revision).unwrap();
    assert!(reopened.get(read.key).unwrap().is_none());
    assert!(reopened.snapshot().documents.is_empty());
    assert_eq!(
        std::fs::metadata(root.join(first.key.relative_path()))
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn concurrent_writers_have_exactly_one_winner() {
    let temp = private_temp();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let original = store.put(&doc(), ExpectedRevision::Absent).unwrap();
    let barrier = Arc::new(Barrier::new(20));
    let threads: Vec<_> = (0..20)
        .map(|i| {
            let barrier = barrier.clone();
            let path = temp.path().to_owned();
            let original = original.clone();
            std::thread::spawn(move || {
                let store = KnowledgeStore::open(&path, false).unwrap();
                let mut doc = original.document;
                doc.body = format!("Writer {i}");
                barrier.wait();
                store
                    .put(&doc, ExpectedRevision::Digest(&original.revision))
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap() as usize)
            .sum::<usize>(),
        1
    );
    let snapshot = store.snapshot();
    assert_eq!(snapshot.documents.len(), 1);
    assert!(snapshot.diagnostics.is_empty());
}

#[test]
fn corrupt_or_mis_scoped_documents_do_not_hide_unaffected_knowledge() {
    let temp = private_temp();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let first = store.put(&doc(), ExpectedRevision::Absent).unwrap();
    let mut second_doc = doc();
    let mut profile = second_doc.profile().unwrap().unwrap();
    profile.id = Uuid::new_v4();
    second_doc.set_profile(&profile).unwrap();
    let second = store.put(&second_doc, ExpectedRevision::Absent).unwrap();
    std::fs::write(temp.path().join(first.key.relative_path()), "corrupt").unwrap();
    let snapshot = store.snapshot();
    assert_eq!(snapshot.documents.len(), 1);
    assert_eq!(snapshot.documents[0].key, second.key);
    assert_eq!(snapshot.diagnostics.len(), 1);
    let bad_path = temp
        .path()
        .join(second.key.relative_path())
        .with_file_name(format!("{}.md", Uuid::new_v4()));
    std::fs::write(bad_path, second.document.serialize().unwrap()).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(snapshot.documents.len(), 1);
    assert_eq!(snapshot.diagnostics.len(), 2);
}

#[test]
fn edits_revocations_and_deletion_are_visible_without_an_index_rebuild() {
    let temp = private_temp();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let corpus = Uuid::new_v4();
    let now = chrono::Utc::now();
    let mut rule = doc();
    rule.activate(corpus, ActivationKind::Reviewed, None, Some(now))
        .unwrap();
    let approved = store.put(&rule, ExpectedRevision::Absent).unwrap();
    assert!(
        store.snapshot().documents[0]
            .document
            .eligible(Some(corpus), now)
            .unwrap()
    );
    rule.body.push_str("Unreviewed change.");
    let edited = store
        .put(&rule, ExpectedRevision::Digest(&approved.revision))
        .unwrap();
    assert!(
        !store.snapshot().documents[0]
            .document
            .eligible(Some(corpus), now)
            .unwrap()
    );
    store.delete(edited.key, &edited.revision).unwrap();
    assert!(store.snapshot().documents.is_empty());
}

#[test]
fn symlinks_cannot_redirect_documents_directories_or_lock_files() {
    let temp = private_temp();
    let outside = private_temp();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let first = store.put(&doc(), ExpectedRevision::Absent).unwrap();
    let target = outside.path().join("untouched");
    std::fs::write(&target, "outside corpus").unwrap();
    let path = temp.path().join(first.key.relative_path());
    std::fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    assert!(store.get(first.key).is_err());
    assert!(
        store
            .put(&doc(), ExpectedRevision::Digest(&first.revision))
            .is_err()
    );
    assert!(store.delete(first.key, &first.revision).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside corpus");
    std::fs::remove_file(&path).unwrap();
    let parent = path.parent().unwrap();
    std::fs::remove_dir(parent).unwrap();
    symlink(outside.path(), parent).unwrap();
    assert!(store.put(&doc(), ExpectedRevision::Absent).is_err());
    assert!(store.get(first.key).is_err());
    std::fs::remove_file(temp.path().join(".writer.lock")).unwrap();
    symlink(&target, temp.path().join(".writer.lock")).unwrap();
    assert!(store.put(&doc(), ExpectedRevision::Absent).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside corpus");
}

#[test]
fn abandoned_staging_files_never_become_documents_and_permissions_are_private() {
    let temp = private_temp();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let first = store.put(&doc(), ExpectedRevision::Absent).unwrap();
    let path = temp.path().join(first.key.relative_path());
    let abandoned = path.with_file_name(format!(".{}.tmp", Uuid::new_v4()));
    std::fs::write(abandoned, "incomplete interrupted write").unwrap();
    drop(store);
    let reopened = KnowledgeStore::open(temp.path(), false).unwrap();
    let snapshot = reopened.snapshot();
    assert_eq!(snapshot.documents.len(), 1);
    assert!(snapshot.diagnostics.is_empty());
    assert_eq!(snapshot.documents[0].revision, first.revision);
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(KnowledgeStore::open(temp.path(), false).is_err());
}

#[test]
fn filenames_are_derived_only_from_validated_typed_identity() {
    let key = Key::from_document(&doc()).unwrap();
    assert_eq!(
        key.relative_path().to_str().unwrap(),
        "repos/9a467418-ddaa-4ee6-b5f2-5b2d2d7d7040/learnings/8d63b13c-9904-428a-a070-c959262b0e54.md"
    );
    let mut malformed = doc();
    malformed.metadata.get_mut("ygg").unwrap()["id"] = "../../escape".into();
    assert!(Key::from_document(&malformed).is_err());
}

// Invoked only by the fault-injection parent, in a separate process so the
// file-size limit and SIGXFSZ handler never affect the test runner.
#[test]
fn failed_write_child() {
    let Some(root) = std::env::var_os("YGG_TEST_LIMITED_WRITE_ROOT") else {
        return;
    };
    let store = KnowledgeStore::open(std::path::Path::new(&root), false).unwrap();
    let original = store
        .get(Key::from_document(&doc()).unwrap())
        .unwrap()
        .unwrap();
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        let limit = libc::rlimit {
            rlim_cur: 64,
            rlim_max: 64,
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
    }
    let mut document = original.document;
    document.body = "must never publish partial contents".repeat(100);
    assert!(
        store
            .put(&document, ExpectedRevision::Digest(&original.revision))
            .is_err()
    );
}

#[test]
fn failed_partial_write_keeps_the_last_acknowledged_revision() {
    let temp = private_temp();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let original = store.put(&doc(), ExpectedRevision::Absent).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "failed_write_child", "--nocapture"])
        .env("YGG_TEST_LIMITED_WRITE_ROOT", temp.path())
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let snapshot = store.snapshot();
    assert_eq!(snapshot.documents.len(), 1);
    assert!(snapshot.diagnostics.is_empty());
    assert_eq!(snapshot.documents[0].revision, original.revision);
    assert_eq!(snapshot.documents[0].document, original.document);
    let parent = temp
        .path()
        .join(original.key.relative_path())
        .parent()
        .unwrap()
        .to_owned();
    assert_eq!(
        std::fs::read_dir(parent).unwrap().count(),
        1,
        "failed staging file should be removed"
    );
}

#[test]
fn concurrent_readers_observe_only_complete_old_or_new_documents() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let temp = private_temp();
    let store = KnowledgeStore::open(temp.path(), false).unwrap();
    let mut initial = doc();
    initial.body = "a".repeat(16_000);
    let mut previous = store.put(&initial, ExpectedRevision::Absent).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let read_done = done.clone();
    let path = temp.path().to_owned();
    let key = previous.key;
    let reader = std::thread::spawn(move || {
        let store = KnowledgeStore::open(&path, false).unwrap();
        let mut count = 0;
        while !read_done.load(Ordering::Acquire) || count == 0 {
            let body = store.get(key).unwrap().unwrap().document.body;
            assert_eq!(body.len(), 16_000);
            assert!(body.bytes().all(|b| b == body.as_bytes()[0]));
            count += 1;
        }
    });
    for i in 0..30 {
        let mut next = previous.document;
        next.body = char::from(b'a' + i % 26).to_string().repeat(16_000);
        previous = store
            .put(&next, ExpectedRevision::Digest(&previous.revision))
            .unwrap();
    }
    done.store(true, Ordering::Release);
    reader.join().unwrap();
}
