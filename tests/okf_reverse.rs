#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::collections::BTreeMap;
use uuid::Uuid;
use ygg::knowledge::{
    document::{ActivationKind, Document, State, digest},
    export::{Entry, Manifest},
    legacy::{self, Mappings},
    reverse,
    store::{ExpectedRevision, Kind, KnowledgeStore},
};
use ygg::models::{learning::Learning, memory::Memory};
fn mappings() -> Mappings {
    Mappings {
        database_id: Uuid::new_v4(),
        corpus_id: Uuid::new_v4(),
        repos: [
            "00000000-0000-0000-0000-000000000100",
            "00000000-0000-0000-0000-000000000200",
        ]
        .into_iter()
        .map(|id| (id.parse().unwrap(), Uuid::new_v4()))
        .collect(),
        users: BTreeMap::from([("".into(), "owner".into())]),
    }
}
fn rules() -> Vec<Learning> {
    serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(include_str!(
            "fixtures/knowledge/learnings.json"
        ))
        .unwrap()["rows"]
            .clone(),
    )
    .unwrap()
}
fn notes() -> Vec<Memory> {
    serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(include_str!("fixtures/knowledge/notes.json"))
            .unwrap()["rows"]
            .clone(),
    )
    .unwrap()
}
fn without_provenance(doc: &mut Document) {
    let mut p = doc.profile().unwrap().unwrap();
    p.extra.remove("legacy");
    p.legacy_repo_id = None;
    doc.set_profile(&p).unwrap();
}
#[test]
fn strict_reverse_adapters_preserve_every_legacy_fixture_and_current_approval() {
    let map = mappings();
    for row in notes() {
        let doc = legacy::import_note(&row, "", &map).unwrap();
        let (out, owner) = legacy::reverse_note(&doc, &map).unwrap();
        assert_eq!(owner, "");
        assert_eq!(
            serde_json::to_value(out).unwrap(),
            serde_json::to_value(row).unwrap()
        );
    }
    for row in rules() {
        let (doc, usage) = legacy::import_learning(&row, "", &map).unwrap();
        let (out, owner) = legacy::reverse_learning(&doc, &usage, &map).unwrap();
        assert_eq!(owner, "");
        assert_eq!(
            serde_json::to_value(out).unwrap(),
            serde_json::to_value(row).unwrap()
        );
    }
    let row = rules().remove(0);
    let (mut doc, usage) = legacy::import_learning(&row, "", &map).unwrap();
    without_provenance(&mut doc);
    let actor = Some(Uuid::new_v4());
    let at = Some(row.created_at);
    doc.activate(map.corpus_id, ActivationKind::Manual, actor, at)
        .unwrap();
    let (out, _) = legacy::reverse_learning(&doc, &usage, &map).unwrap();
    assert_eq!((out.approved_by, out.approved_at), (actor, at));
    assert_eq!(out.status, "active");
    let scoped = rules().into_iter().find(|r| r.repo_id.is_some()).unwrap();
    let (mut moved, usage) = legacy::import_learning(&scoped, "", &map).unwrap();
    let target = *map
        .repos
        .keys()
        .find(|id| Some(**id) != scoped.repo_id)
        .unwrap();
    let mut p = moved.profile().unwrap().unwrap();
    p.repo = Some(map.repos[&target]);
    p.state = State::Pending;
    p.approval = None;
    moved.set_profile(&p).unwrap();
    let (out, _) = legacy::reverse_learning(&moved, &usage, &map).unwrap();
    assert_eq!(out.repo_id, Some(target));
    assert_eq!(out.status, "pending");
}
#[test]
fn rollback_rejects_lossy_metadata_invalid_approvals_and_ambiguous_owners() {
    let mut map = mappings();
    let row = rules().remove(0);
    let (doc, usage) = legacy::import_learning(&row, "", &map).unwrap();
    let mut edited = doc.clone();
    edited.body.push_str("changed");
    assert!(legacy::reverse_learning(&edited, &usage, &map).is_err());
    let mut p = edited.profile().unwrap().unwrap();
    p.state = State::Pending;
    p.approval = None;
    edited.set_profile(&p).unwrap();
    assert_eq!(
        legacy::reverse_learning(&edited, &usage, &map)
            .unwrap()
            .0
            .status,
        "pending"
    );
    let mut unknown = doc.clone();
    unknown
        .metadata
        .insert("stale_after".into(), "2030-01-01T00:00:00Z".into());
    assert!(legacy::reverse_learning(&unknown, &usage, &map).is_err());
    let mut unknown = doc.clone();
    let mut p = unknown.profile().unwrap().unwrap();
    p.extra.insert("future_policy".into(), true.into());
    unknown.set_profile(&p).unwrap();
    assert!(legacy::reverse_learning(&unknown, &usage, &map).is_err());
    let mut unknown = doc.clone();
    let mut p = unknown.profile().unwrap().unwrap();
    p.extra
        .get_mut("legacy")
        .unwrap()
        .as_mapping_mut()
        .unwrap()
        .insert("future_field".into(), true.into());
    unknown.set_profile(&p).unwrap();
    assert!(legacy::reverse_learning(&unknown, &usage, &map).is_err());
    let mut precise = row.clone();
    precise.created_at += chrono::Duration::nanoseconds(1);
    let (precise_doc, precise_usage) = legacy::import_learning(&precise, "", &map).unwrap();
    assert!(legacy::reverse_learning(&precise_doc, &precise_usage, &map).is_err());
    precise.created_at = "2016-12-31T23:59:60Z".parse().unwrap();
    let (precise_doc, precise_usage) = legacy::import_learning(&precise, "", &map).unwrap();
    assert!(legacy::reverse_learning(&precise_doc, &precise_usage, &map).is_err());
    let mut new = doc.clone();
    without_provenance(&mut new);
    map.users.insert("other".into(), "owner".into());
    assert!(legacy::reverse_learning(&new, &usage, &map).is_err());
}
#[test]
fn candidate_uses_current_edits_deletions_new_documents_and_current_usage() {
    let map = mappings();
    let temp = tempfile::tempdir().unwrap();
    let store = KnowledgeStore::open(&temp.path().join("bundle"), true).unwrap();
    let a = store
        .put(
            &legacy::import_note(&notes()[0], "", &map).unwrap(),
            ExpectedRevision::Absent,
        )
        .unwrap();
    let b = store
        .put(
            &legacy::import_note(&notes()[1], "", &map).unwrap(),
            ExpectedRevision::Absent,
        )
        .unwrap();
    let (rule, baseline) = legacy::import_learning(&rules()[0], "", &map).unwrap();
    let r = store.put(&rule, ExpectedRevision::Absent).unwrap();
    let original = Manifest {
        version: 1,
        database_id: map.database_id,
        generation: 2,
        corpus_id: map.corpus_id,
        mappings: serde_json::to_value(&map).unwrap(),
        entries: store
            .snapshot()
            .documents
            .iter()
            .map(|d| Entry {
                key: d.key,
                source_digest: digest(b"SQL fixture row"),
                document_digest: d.revision.clone(),
                usage: (d.key.kind == Kind::Learning).then_some(baseline.clone()),
            })
            .collect(),
    };
    let mut changed = a.document;
    changed.body = "post-cutover edit λ\n".into();
    store
        .put(&changed, ExpectedRevision::Digest(&a.revision))
        .unwrap();
    store.delete(b.key, &b.revision).unwrap();
    let mut changed = r.document;
    changed.body = "reviewed edit\n".into();
    let actor = Some(Uuid::new_v4());
    let at = Some(rules()[0].created_at);
    changed
        .activate(map.corpus_id, ActivationKind::Reviewed, actor, at)
        .unwrap();
    store
        .put(&changed, ExpectedRevision::Digest(&r.revision))
        .unwrap();
    let mut new_row = notes()[0].clone();
    new_row.memory_id = Uuid::new_v4();
    new_row.text = "new note".into();
    let mut new = legacy::import_note(&new_row, "", &map).unwrap();
    without_provenance(&mut new);
    store.put(&new, ExpectedRevision::Absent).unwrap();
    let mut new_row = rules()[0].clone();
    new_row.learning_id = Uuid::new_v4();
    new_row.text = "new manual rule".into();
    let (mut new, new_usage) = legacy::import_learning(&new_row, "", &map).unwrap();
    without_provenance(&mut new);
    new.activate(map.corpus_id, ActivationKind::Manual, actor, at)
        .unwrap();
    store.put(&new, ExpectedRevision::Absent).unwrap();
    let mut totals = baseline.clone();
    totals.applied_count = 9;
    totals.last_applied_at = at;
    let mut usage = BTreeMap::from([
        (totals.document_id, totals),
        (new_usage.document_id, new_usage),
    ]);
    let current = store.snapshot();
    let candidate = reverse::build(&original, &current, &usage).unwrap();
    assert_eq!(
        (
            candidate.notes.len(),
            candidate.learnings.len(),
            candidate.documents.len()
        ),
        (2, 2, 4)
    );
    assert_eq!(candidate.deleted, vec![b.key]);
    assert!(
        candidate
            .notes
            .iter()
            .any(|r| r["text"] == "post-cutover edit λ\n" && r["user_id"] == "")
    );
    let row = candidate
        .learnings
        .iter()
        .find(|r| r["learning_id"] == r#"00000000-0000-0000-0000-000000000001"#)
        .unwrap();
    assert_eq!(row["text"], "reviewed edit\n");
    assert_eq!(row["applied_count"], 9);
    assert_eq!(row["status"], "active");
    assert_eq!(row["approved_by"], actor.unwrap().to_string());
    let old_id = rules()[0].learning_id;
    usage.get_mut(&old_id).unwrap().applied_count = -1;
    assert!(reverse::build(&original, &current, &usage).is_err());
    usage.get_mut(&old_id).unwrap().applied_count = 9;
    usage.remove(&new_row.learning_id);
    assert!(reverse::build(&original, &current, &usage).is_err());
    let mut incomplete = current;
    incomplete.diagnostics.push("unreadable file".into());
    assert!(reverse::build(&original, &incomplete, &usage).is_err());
}
