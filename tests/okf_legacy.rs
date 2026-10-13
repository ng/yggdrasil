use std::collections::BTreeMap;
use uuid::Uuid;
use ygg::{
    knowledge::{
        document::{ActivationKind, Scope, State},
        legacy::{self, Mappings},
    },
    models::{learning::Learning, memory::Memory},
};

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
        users: BTreeMap::from([(String::new(), "explicit-owner".into())]),
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
#[test]
fn every_legacy_fixture_round_trips_exact_model_json_and_explicit_owner() {
    let map = mappings();
    for row in rules() {
        let (doc, usage) = legacy::import_learning(&row, "", &map).unwrap();
        assert_eq!(doc.body, row.text);
        assert_eq!(legacy::legacy_user_id(&doc, &map).unwrap(), "");
        assert_eq!(
            serde_json::to_value(legacy::learning_json_model(&doc, &usage, &map).unwrap()).unwrap(),
            serde_json::to_value(&row).unwrap()
        );
        assert_eq!(
            doc.activation_valid(map.corpus_id).unwrap(),
            row.status == "active"
        );
        if row.status == "active" {
            assert_eq!(
                doc.profile().unwrap().unwrap().approval.unwrap().kind,
                ActivationKind::Legacy
            );
        }
    }
    let notes: Vec<Memory> = serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(include_str!("fixtures/knowledge/notes.json"))
            .unwrap()["rows"]
            .clone(),
    )
    .unwrap();
    for row in notes {
        let doc = legacy::import_note(&row, "", &map).unwrap();
        assert_eq!(legacy::legacy_user_id(&doc, &map).unwrap(), "");
        assert_eq!(
            serde_json::to_value(legacy::note_json_model(&doc, &map).unwrap()).unwrap(),
            serde_json::to_value(row).unwrap()
        );
    }
}
#[test]
fn missing_ownership_scope_and_unknown_states_are_errors() {
    let mut map = mappings();
    let mut row = rules().remove(1);
    assert!(legacy::import_learning(&row, "unmapped", &map).is_err());
    map.users.clear();
    assert!(legacy::import_learning(&row, "", &map).is_err());
    map.users.insert("".into(), "owner".into());
    map.repos.clear();
    assert!(legacy::import_learning(&row, "", &map).is_err());
    row.repo_id = None;
    row.status = "unknown".into();
    assert!(legacy::import_learning(&row, "", &map).is_err());
    row.status = "active".into();
    row.source = "unknown".into();
    assert!(legacy::import_learning(&row, "", &map).is_err());
}
#[test]
fn duplicate_legacy_scopes_round_trip_but_moves_and_rebinding_use_current_scope() {
    let mut map = mappings();
    let row = rules().remove(1);
    let old = row.repo_id.unwrap();
    let portable = map.repos[&old];
    let duplicate = Uuid::new_v4();
    map.repos.insert(duplicate, portable);
    let (mut doc, usage) = legacy::import_learning(&row, "", &map).unwrap();
    assert_eq!(
        legacy::learning_json_model(&doc, &usage, &map)
            .unwrap()
            .repo_id,
        Some(old)
    );
    let target_legacy = "00000000-0000-0000-0000-000000000200".parse().unwrap();
    let mut p = doc.profile().unwrap().unwrap();
    p.repo = Some(map.repos[&target_legacy]);
    p.state = State::Pending;
    p.approval = None;
    doc.set_profile(&p).unwrap();
    assert_eq!(
        legacy::learning_json_model(&doc, &usage, &map)
            .unwrap()
            .repo_id,
        Some(target_legacy)
    );
    p.scope = Scope::Global;
    p.repo = None;
    doc.set_profile(&p).unwrap();
    assert!(
        legacy::learning_json_model(&doc, &usage, &map)
            .unwrap()
            .repo_id
            .is_none()
    );
    p.scope = Scope::Repo;
    p.repo = Some(portable);
    doc.set_profile(&p).unwrap();
    map.database_id = Uuid::new_v4();
    assert!(legacy::learning_json_model(&doc, &usage, &map).is_err());
    map.repos.remove(&old);
    assert_eq!(
        legacy::learning_json_model(&doc, &usage, &map)
            .unwrap()
            .repo_id,
        Some(duplicate)
    );
}
#[test]
fn raw_json_tags_and_pending_approval_history_survive_without_activation() {
    let map = mappings();
    let mut row = rules().remove(0);
    row.status = "pending".into();
    row.approved_by = Some(Uuid::new_v4());
    row.approved_at = Some(row.created_at);
    for tags in [
        serde_json::Value::Null,
        serde_json::json!(["a", 2]),
        serde_json::json!(true),
        serde_json::json!(4),
    ] {
        row.scope_tags = tags;
        let (doc, usage) = legacy::import_learning(&row, "", &map).unwrap();
        assert!(!doc.activation_valid(map.corpus_id).unwrap());
        assert!(doc.profile().unwrap().unwrap().approval.is_none());
        assert_eq!(
            serde_json::to_value(legacy::learning_json_model(&doc, &usage, &map).unwrap()).unwrap(),
            serde_json::to_value(&row).unwrap()
        );
    }
}
#[test]
fn invalidated_approval_is_pending_and_usage_cannot_cross_identities() {
    let map = mappings();
    let mut row = rules().remove(0);
    row.approved_at = Some(row.created_at);
    row.approved_by = Some(Uuid::new_v4());
    let (mut doc, mut usage) = legacy::import_learning(&row, "", &map).unwrap();
    let digest = doc.approval_digest().unwrap();
    usage.applied_count += 7;
    assert_eq!(
        legacy::learning_json_model(&doc, &usage, &map)
            .unwrap()
            .applied_count,
        row.applied_count + 7
    );
    assert_eq!(doc.approval_digest().unwrap(), digest);
    doc.body.push_str("changed");
    let api = legacy::learning_json_model(&doc, &usage, &map).unwrap();
    assert_eq!(api.status, "pending");
    assert!(api.approved_by.is_none());
    assert!(api.approved_at.is_none());
    let mut p = doc.profile().unwrap().unwrap();
    p.state = State::Pending;
    p.approval = None;
    doc.set_profile(&p).unwrap();
    assert!(
        legacy::learning_json_model(&doc, &usage, &map)
            .unwrap()
            .approved_at
            .is_none()
    );
    usage.document_id = Uuid::new_v4();
    assert!(legacy::learning_json_model(&doc, &usage, &map).is_err());
}
