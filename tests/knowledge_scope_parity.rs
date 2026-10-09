//! Compare actual SQL repositories with imported OKF retrieval, including order
//! and legacy JSON. SQL writes use connection-local temporary fixture tables.
#![cfg(any(target_os = "macos", target_os = "linux"))]
use chrono::Utc;
use serde::Deserialize;
use std::collections::BTreeMap;
use uuid::Uuid;
use ygg::{
    knowledge::{
        identity::{GitIdentity, IdentityRegistry},
        legacy::{self, Mappings, Usage},
        matching::Filters,
        service::KnowledgeService,
        store::{ExpectedRevision, KnowledgeStore, Snapshot},
    },
    models::{
        learning::{Learning, LearningRepo},
        memory::{Memory, MemoryRepo},
    },
};
#[derive(Deserialize)]
struct LearningCase {
    name: String,
    repo: Option<Uuid>,
    file: Option<String>,
    rule: Option<String>,
    agent: Option<String>,
    kind: Option<String>,
}
#[derive(Deserialize)]
struct NoteCase {
    repo: Option<Uuid>,
    all: bool,
    limit: i64,
}
fn notes(snapshot: Snapshot, map: &Mappings) -> serde_json::Value {
    assert!(
        snapshot.diagnostics.is_empty(),
        "{:?}",
        snapshot.diagnostics
    );
    serde_json::to_value(
        snapshot
            .documents
            .iter()
            .map(|d| legacy::note_json_model(&d.document, map).unwrap())
            .collect::<Vec<_>>(),
    )
    .unwrap()
}
fn rules(snapshot: Snapshot, map: &Mappings, usage: &BTreeMap<Uuid, Usage>) -> serde_json::Value {
    assert!(
        snapshot.diagnostics.is_empty(),
        "{:?}",
        snapshot.diagnostics
    );
    serde_json::to_value(
        snapshot
            .documents
            .iter()
            .map(|d| legacy::learning_json_model(&d.document, &usage[&d.key.id], map).unwrap())
            .collect::<Vec<_>>(),
    )
    .unwrap()
}
#[tokio::test]
async fn sql_and_imported_okf_scope_results_and_json_match() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required"))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TEMP TABLE learnings (LIKE public.learnings INCLUDING DEFAULTS); CREATE TEMP TABLE memories (LIKE public.memories INCLUDING DEFAULTS);").execute(&pool).await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let registry = IdentityRegistry::open(&temp.path().join("policy"), true).unwrap();
    let identity = registry.initialize(true).unwrap();
    let mut map = Mappings {
        database_id: Uuid::new_v4(),
        corpus_id: identity.corpus_id,
        repos: BTreeMap::new(),
        users: BTreeMap::from([(String::new(), "fixture-owner".into())]),
    };
    // Include the queried empty repository; absence of documents is distinct
    // from absence of an explicitly mapped repository identity.
    for id in [0x100, 0x200, 0x300] {
        let portable = registry
            .bind(&GitIdentity {
                common_dir: temp.path().join(format!("repo-{id}/.git")),
                origin: None,
            })
            .unwrap();
        map.repos.insert(Uuid::from_u128(id), portable);
    }
    let store = KnowledgeStore::open(&temp.path().join("bundle"), true).unwrap();
    let learning_fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/learnings.json")).unwrap();
    let note_fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/notes.json")).unwrap();
    let mut usage = BTreeMap::new();
    for source in learning_fixture["rows"].as_array().unwrap() {
        let row: Learning = serde_json::from_value(source.clone()).unwrap();
        let mut sql = source.clone();
        sql["user_id"] = "".into();
        sqlx::query(
            "INSERT INTO learnings SELECT * FROM jsonb_populate_record(NULL::learnings, $1)",
        )
        .bind(sql)
        .execute(&pool)
        .await
        .unwrap();
        let (doc, count) = legacy::import_learning(&row, "", &map).unwrap();
        usage.insert(row.learning_id, count);
        store.put(&doc, ExpectedRevision::Absent).unwrap();
    }
    for source in note_fixture["rows"].as_array().unwrap() {
        let row: Memory = serde_json::from_value(source.clone()).unwrap();
        let mut sql = source.clone();
        sql["user_id"] = "".into();
        sqlx::query("INSERT INTO memories SELECT * FROM jsonb_populate_record(NULL::memories, $1)")
            .bind(sql)
            .execute(&pool)
            .await
            .unwrap();
        store
            .put(
                &legacy::import_note(&row, "", &map).unwrap(),
                ExpectedRevision::Absent,
            )
            .unwrap();
    }
    let service = KnowledgeService::new(store, registry, "fixture-owner".into()).unwrap();
    // Execute twice: fresh index then warm cached candidates must match SQL.
    for _ in 0..2 {
        for case in
            serde_json::from_value::<Vec<LearningCase>>(learning_fixture["cases"].clone()).unwrap()
        {
            let expected = serde_json::to_value(
                LearningRepo::new(&pool)
                    .list_matching(
                        case.repo,
                        case.file.as_deref(),
                        case.rule.as_deref(),
                        case.agent.as_deref(),
                        case.kind.as_deref(),
                    )
                    .await
                    .unwrap(),
            )
            .unwrap();
            let filters = Filters {
                repo: case.repo.map(|id| map.repos[&id]),
                file: case.file.as_deref(),
                rule: case.rule.as_deref(),
                agent: case.agent.as_deref(),
                kind: case.kind.as_deref(),
            };
            assert_eq!(
                rules(service.list_rules(&filters).unwrap(), &map, &usage),
                expected,
                "browse: {}",
                case.name
            );
            assert_eq!(
                rules(service.rules(&filters, Utc::now()).unwrap(), &map, &usage),
                expected,
                "injection: {}",
                case.name
            );
        }
        for case in serde_json::from_value::<Vec<NoteCase>>(note_fixture["cases"].clone()).unwrap()
        {
            let expected = serde_json::to_value(
                MemoryRepo::new(&pool)
                    .list(case.repo, case.all, case.limit)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let repo = case.repo.map(|id| map.repos[&id]);
            assert_eq!(
                notes(
                    service.notes(repo, case.all, case.limit as usize).unwrap(),
                    &map
                ),
                expected
            );
            if !case.all && case.limit == 5 {
                assert_eq!(
                    notes(service.prime_notes(repo, Utc::now()).unwrap(), &map),
                    expected
                );
            }
        }
        for repo in [
            None,
            Some(Uuid::from_u128(0x100)),
            Some(Uuid::from_u128(0x200)),
        ] {
            let expected =
                serde_json::to_value(LearningRepo::new(&pool).list_pending(repo).await.unwrap())
                    .unwrap();
            assert_eq!(
                rules(
                    service.pending(repo.map(|id| map.repos[&id])).unwrap(),
                    &map,
                    &usage
                ),
                expected
            );
        }
    }
    pool.close().await;
}
