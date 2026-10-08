#![cfg(any(target_os = "macos", target_os = "linux"))]
use chrono::{TimeZone, Utc};
use std::{fs, os::unix::fs::symlink};
use uuid::Uuid;
use ygg::knowledge::{
    document::ActivationKind,
    identity::{GitIdentity, IdentityRegistry},
    matching::Filters,
    service::{Creation, KnowledgeService, RuleInput},
    store::{KnowledgeStore, RevisionedDocument},
};
fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 0, 0, 0).unwrap()
}
fn reopen(root: &std::path::Path) -> KnowledgeService {
    KnowledgeService::new(
        KnowledgeStore::open(&root.join("bundle"), false).unwrap(),
        IdentityRegistry::open(&root.join("policy"), false).unwrap(),
        "index-user".into(),
    )
    .unwrap()
}
fn setup() -> (
    tempfile::TempDir,
    KnowledgeService,
    Uuid,
    RevisionedDocument,
) {
    let temp = tempfile::tempdir().unwrap();
    let policy = IdentityRegistry::open(&temp.path().join("policy"), true).unwrap();
    policy.initialize(true).unwrap();
    let repo = policy
        .bind(&GitIdentity {
            common_dir: temp.path().canonicalize().unwrap().join("repo.git"),
            origin: None,
        })
        .unwrap();
    KnowledgeStore::open(&temp.path().join("bundle"), true).unwrap();
    let service = reopen(temp.path());
    let rule = service
        .create_rule(
            RuleInput {
                repo: Some(repo),
                rule_id: Some("alpha".into()),
                text: "rule body".into(),
                ..RuleInput::default()
            },
            Creation::ManualActive,
            now(),
        )
        .unwrap();
    (temp, service, repo, rule)
}
#[test]
fn reopening_reuses_index_and_direct_edits_refresh_matching_metadata() {
    let (temp, service, repo, rule) = setup();
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    let index = temp.path().join("bundle/.lookup.json");
    let cache = fs::read(&index).unwrap();
    let modified = fs::metadata(&index).unwrap().modified().unwrap();
    drop(service);
    let service = reopen(temp.path());
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    assert_eq!(fs::read(&index).unwrap(), cache);
    assert_eq!(fs::metadata(&index).unwrap().modified().unwrap(), modified);
    let path = temp.path().join("bundle").join(rule.key.relative_path());
    let stamp = fs::metadata(&path).unwrap().modified().unwrap();
    let mut document = rule.document;
    let mut profile = document.profile().unwrap().unwrap();
    let corpus = profile.approval.as_ref().unwrap().corpus_id;
    profile.rule_id = Some("bravo".into());
    document.set_profile(&profile).unwrap();
    document
        .activate(corpus, ActivationKind::Manual, None, Some(now()))
        .unwrap();
    let text = document.serialize().unwrap();
    assert_eq!(text.len(), fs::read(&path).unwrap().len());
    fs::write(&path, text).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(stamp)
        .unwrap();
    let filters = Filters {
        repo: Some(repo),
        rule: Some("bravo"),
        ..Filters::default()
    };
    assert_eq!(service.rules(&filters, now()).unwrap().documents.len(), 1);
    assert!(
        service
            .rules(
                &Filters {
                    rule: Some("alpha"),
                    ..filters
                },
                now()
            )
            .unwrap()
            .documents
            .is_empty()
    );
}
#[test]
fn corrupt_deleted_and_old_parser_indexes_rebuild_without_losing_documents() {
    let (temp, service, _, _) = setup();
    let index = temp.path().join("bundle/.lookup.json");
    for contents in [
        "broken",
        "{\"version\":1,\"parser\":999,\"corpus_revision\":\"x\"}\n{}",
    ] {
        fs::write(&index, contents).unwrap();
        let current = service.rules(&Filters::default(), now()).unwrap();
        assert_eq!(current.documents.len(), 1);
        assert!(current.diagnostics.is_empty());
    }
    fs::remove_file(index).unwrap();
    assert_eq!(
        reopen(temp.path())
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
}
#[test]
fn cached_rows_never_resurrect_deleted_corrupt_symlinked_or_duplicate_files() {
    let (temp, service, _, rule) = setup();
    let root = temp.path().join("bundle");
    let path = root.join(rule.key.relative_path());
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    let duplicate = root.join("global/notes");
    fs::create_dir_all(&duplicate).unwrap();
    fs::copy(&path, duplicate.join(format!("{}.md", rule.key.id))).unwrap();
    assert!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .is_empty()
    );
    fs::remove_dir_all(duplicate).unwrap();
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    fs::write(&path, "corrupt").unwrap();
    assert!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .is_empty()
    );
    fs::write(&path, rule.document.serialize().unwrap()).unwrap();
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    fs::remove_file(&path).unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), rule.document.serialize().unwrap()).unwrap();
    symlink(outside.path(), &path).unwrap();
    assert!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .is_empty()
    );
    fs::remove_file(&path).unwrap();
    assert!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .is_empty()
    );
}
#[test]
fn unwritable_cache_location_does_not_prevent_valid_reads() {
    let (temp, service, _, _) = setup();
    fs::create_dir(temp.path().join("bundle/.lookup.json")).unwrap();
    let result = service.rules(&Filters::default(), now()).unwrap();
    assert_eq!(result.documents.len(), 1);
    assert!(result.diagnostics.is_empty());
}

#[test]
fn internally_consistent_cache_cannot_authorize_a_nonmatching_rule() {
    let (temp, service, repo, _) = setup();
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    let path = temp.path().join("bundle/.lookup.json");
    let text = fs::read_to_string(&path).unwrap();
    let (header, body) = text.split_once('\n').unwrap();
    let mut header: serde_json::Value = serde_json::from_str(header).unwrap();
    let mut rows: serde_json::Value = serde_json::from_str(body).unwrap();
    let row = rows.as_object_mut().unwrap().values_mut().next().unwrap();
    row["profile"]["rule_id"] = "bravo".into();
    let body = serde_json::to_string(&rows).unwrap();
    header["corpus_revision"] = ygg::knowledge::document::digest(body.as_bytes()).into();
    fs::write(
        path,
        format!("{}\n{body}", serde_json::to_string(&header).unwrap()),
    )
    .unwrap();
    let filters = Filters {
        repo: Some(repo),
        rule: Some("bravo"),
        ..Filters::default()
    };
    assert!(service.rules(&filters, now()).unwrap().documents.is_empty());
}
