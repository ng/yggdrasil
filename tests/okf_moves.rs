#![cfg(any(target_os = "macos", target_os = "linux"))]
use chrono::Utc;
use std::sync::{Arc, Barrier};
use uuid::Uuid;
use ygg::knowledge::{
    document::State,
    identity::{GitIdentity, IdentityRegistry},
    matching::Filters,
    service::{Approver, Creation, KnowledgeService, RuleInput},
    store::{ExpectedRevision, Key, KnowledgeStore},
};

fn service(root: &std::path::Path) -> KnowledgeService {
    KnowledgeService::new(
        KnowledgeStore::open(&root.join("bundle"), true).unwrap(),
        IdentityRegistry::open(&root.join("policy"), false).unwrap(),
        "test-user".into(),
    )
    .unwrap()
}
fn setup() -> (tempfile::TempDir, KnowledgeService, Uuid, Uuid) {
    let temp = tempfile::tempdir().unwrap();
    let registry = IdentityRegistry::open(&temp.path().join("policy"), true).unwrap();
    registry.initialize(true).unwrap();
    let root = temp.path().canonicalize().unwrap();
    let a = registry
        .bind(&GitIdentity {
            common_dir: root.join("a/.git"),
            origin: None,
        })
        .unwrap();
    let b = registry
        .bind(&GitIdentity {
            common_dir: root.join("b/.git"),
            origin: None,
        })
        .unwrap();
    let service = service(temp.path());
    (temp, service, a, b)
}

#[test]
fn rule_moves_preserve_identity_and_provenance_but_invalidate_approval() {
    let (temp, service, a, b) = setup();
    let now = Utc::now();
    let original = service
        .create_rule(
            RuleInput {
                repo: Some(a),
                text: "Rule body λ\n".into(),
                created_by: Some(Uuid::new_v4()),
                ..RuleInput::default()
            },
            Creation::ManualActive,
            now,
        )
        .unwrap();
    let global = service
        .move_scope(original.key.id, &original.revision, None)
        .unwrap();
    assert_eq!(global.key.id, original.key.id);
    assert_eq!(global.document.body, original.document.body);
    let p = global.document.profile().unwrap().unwrap();
    let old = original.document.profile().unwrap().unwrap();
    assert_eq!(p.created_at, old.created_at);
    assert_eq!(p.created_by, old.created_by);
    assert_eq!(p.source, old.source);
    assert_eq!(p.state, State::Pending);
    assert!(p.approval.is_none());
    assert!(
        service
            .rules(&Filters::default(), now)
            .unwrap()
            .documents
            .is_empty()
    );
    assert!(
        !temp
            .path()
            .join("bundle")
            .join(original.key.relative_path())
            .exists()
    );
    assert!(
        service
            .revalidate_rule(&original, &Filters::default(), now)
            .unwrap()
            .is_none()
    );
    let approved = service
        .approve(global.key.id, &global.revision, Approver::Human(None), now)
        .unwrap();
    let moved = service
        .move_scope(approved.key.id, &approved.revision, Some(b))
        .unwrap();
    assert_eq!(moved.key.repo, Some(b));
    assert_eq!(service.pending(Some(b)).unwrap().documents.len(), 1);
    assert!(service.pending(Some(a)).unwrap().documents.is_empty());
    assert!(
        service
            .rules(&Filters::default(), now)
            .unwrap()
            .documents
            .is_empty()
    );
}

#[test]
fn note_moves_are_conditional_and_never_fall_back_to_unknown_scope() {
    let (_temp, service, a, b) = setup();
    let note = service
        .create_note(Some(a), "Note".into(), None, Utc::now())
        .unwrap();
    assert!(service.move_scope(note.key.id, "stale", Some(b)).is_err());
    assert!(
        service
            .move_scope(note.key.id, &note.revision, Some(Uuid::new_v4()))
            .is_err()
    );
    assert_eq!(
        service.get(note.key.id).unwrap().unwrap().revision,
        note.revision
    );
    let moved = service
        .move_scope(note.key.id, &note.revision, Some(b))
        .unwrap();
    assert!(
        service
            .notes(Some(a), false, 5)
            .unwrap()
            .documents
            .is_empty()
    );
    assert_eq!(service.notes(Some(b), false, 5).unwrap().documents.len(), 1);
    assert_eq!(
        moved.document.profile().unwrap().unwrap().state,
        State::Active
    );
    assert!(
        service
            .move_scope(note.key.id, &note.revision, None)
            .is_err()
    );
}

#[test]
fn simultaneous_scope_changes_have_one_acknowledged_winner() {
    let (temp, svc, a, b) = setup();
    let note = svc
        .create_note(Some(a), "Original".into(), None, Utc::now())
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for target in [None, Some(b)] {
        let root = temp.path().to_owned();
        let barrier = barrier.clone();
        let note = note.clone();
        threads.push(std::thread::spawn(move || {
            let svc = service(&root);
            barrier.wait();
            svc.move_scope(note.key.id, &note.revision, target).is_ok()
        }));
    }
    assert_eq!(
        threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let snapshot = KnowledgeStore::open(&temp.path().join("bundle"), false)
        .unwrap()
        .snapshot();
    assert!(snapshot.diagnostics.is_empty());
    assert_eq!(snapshot.documents.len(), 1);
    assert_eq!(snapshot.documents[0].key.id, note.key.id);
}

#[test]
fn collisions_and_attempts_to_preserve_rule_activation_are_rejected() {
    let (temp, svc, a, _) = setup();
    let first = svc
        .create_rule(
            RuleInput {
                repo: Some(a),
                text: "rule".into(),
                ..RuleInput::default()
            },
            Creation::ManualActive,
            Utc::now(),
        )
        .unwrap();
    let store = KnowledgeStore::open(&temp.path().join("bundle"), false).unwrap();
    let mut moved = first.document.clone();
    let mut p = moved.profile().unwrap().unwrap();
    p.repo = None;
    p.scope = ygg::knowledge::document::Scope::Global;
    moved.set_profile(&p).unwrap();
    assert!(
        store
            .move_document(first.key, &moved, &first.revision)
            .is_err()
    );
    p.state = State::Pending;
    p.approval = None;
    moved.set_profile(&p).unwrap();
    let collision = temp
        .path()
        .join("bundle")
        .join(Key::from_document(&moved).unwrap().relative_path());
    std::fs::create_dir_all(collision.parent().unwrap()).unwrap();
    std::fs::write(&collision, "independent contents").unwrap();
    assert!(
        store
            .move_document(first.key, &moved, &first.revision)
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(collision).unwrap(),
        "independent contents"
    );
    assert_eq!(
        store.get(first.key).unwrap().unwrap().revision,
        first.revision
    );
    // The ordinary write API still cannot create a second scoped copy.
    assert!(store.put(&moved, ExpectedRevision::Absent).is_err());
}
