//! Baseline SQL and JSON contracts consumed by the later OKF adapters.
//! DATABASE_URL must point to an isolated migrated test database. All writes
//! below use connection-local temporary tables (never the user's corpus).
use serde::Deserialize;
use uuid::Uuid;
use ygg::models::{
    learning::{Learning, LearningRepo},
    memory::{Memory, MemoryRepo},
};

#[derive(Deserialize)]
struct LearningCase {
    name: String,
    repo: Option<Uuid>,
    file: Option<String>,
    rule: Option<String>,
    agent: Option<String>,
    kind: Option<String>,
    ids: Vec<u128>,
}

#[derive(Deserialize)]
struct NoteCase {
    repo: Option<Uuid>,
    all: bool,
    limit: i64,
    ids: Vec<u128>,
}

async fn pool() -> sqlx::PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required"))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TEMP TABLE learnings (LIKE public.learnings INCLUDING DEFAULTS); CREATE TEMP TABLE memories (LIKE public.memories INCLUDING DEFAULTS);")
        .execute(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn legacy_learning_matching_and_approval_contract() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/learnings.json")).unwrap();
    let pool = pool().await;
    for row in fixture["rows"].as_array().unwrap() {
        let mut source = row.clone();
        source["user_id"] = "".into();
        sqlx::query(
            "INSERT INTO learnings SELECT * FROM jsonb_populate_record(NULL::learnings, $1)",
        )
        .bind(&source)
        .execute(&pool)
        .await
        .unwrap();
        let expected = row.clone();
        let row: Learning = serde_json::from_value(row.clone()).unwrap();
        let actual: Learning = sqlx::query_as("SELECT * FROM learnings WHERE learning_id=$1")
            .bind(row.learning_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }
    let repo = LearningRepo::new(&pool);
    for case in serde_json::from_value::<Vec<LearningCase>>(fixture["cases"].clone()).unwrap() {
        let actual = repo
            .list_matching(
                case.repo,
                case.file.as_deref(),
                case.rule.as_deref(),
                case.agent.as_deref(),
                case.kind.as_deref(),
            )
            .await
            .unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|r| r.learning_id.as_u128())
                .collect::<Vec<_>>(),
            case.ids,
            "{}",
            case.name
        );
    }
    let pending = repo.list_pending(None).await.unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|r| r.learning_id.as_u128())
            .collect::<Vec<_>>(),
        [6]
    );
    assert!(!repo.reject(Uuid::from_u128(1)).await.unwrap());
    let approved = repo
        .approve(Uuid::from_u128(6), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(approved.status, "active");
    assert_eq!(approved.source, "proposed");
    assert!(approved.approved_at.is_some());
    assert!(approved.approved_by.is_none()); // Preserve unknown actor; never invent verification.
    assert!(
        repo.approve(Uuid::from_u128(6), None)
            .await
            .unwrap()
            .is_none()
    );
    pool.close().await;
}

#[tokio::test]
async fn legacy_note_scope_prime_limit_and_json_contract() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/notes.json")).unwrap();
    let pool = pool().await;
    for row in fixture["rows"].as_array().unwrap() {
        let mut source = row.clone();
        source["user_id"] = "".into();
        sqlx::query("INSERT INTO memories SELECT * FROM jsonb_populate_record(NULL::memories, $1)")
            .bind(&source)
            .execute(&pool)
            .await
            .unwrap();
        let expected = row.clone();
        let row: Memory = serde_json::from_value(row.clone()).unwrap();
        let actual: Memory = sqlx::query_as("SELECT * FROM memories WHERE memory_id=$1")
            .bind(row.memory_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }
    for case in serde_json::from_value::<Vec<NoteCase>>(fixture["cases"].clone()).unwrap() {
        let actual = MemoryRepo::new(&pool)
            .list(case.repo, case.all, case.limit)
            .await
            .unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|r| r.memory_id.as_u128() & 0xffffffffffff)
                .collect::<Vec<_>>(),
            case.ids
        );
    }
    pool.close().await;
}
