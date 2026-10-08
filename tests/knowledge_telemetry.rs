//! Isolated-schema SQL tests; DATABASE_URL must name a disposable test database.
use chrono::{Duration, TimeZone, Utc};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;
use ygg::knowledge::{legacy::Usage, telemetry::Telemetry};

struct Fixture {
    admin: PgPool,
    pool: PgPool,
    schema: String,
}
impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("telemetry_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(24)
            .after_connect(move |connection, _| {
                let query = format!("SET search_path TO {path}, public");
                Box::pin(async move {
                    sqlx::query(&query).execute(connection).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20261008000001_knowledge_telemetry.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        Self {
            admin,
            pool,
            schema,
        }
    }
    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn baseline() -> Usage {
    Usage {
        corpus_id: Uuid::new_v4(),
        document_id: Uuid::new_v4(),
        applied_count: 7,
        last_applied_at: Some(Utc.with_ymd_and_hms(2026, 10, 8, 0, 0, 0).unwrap()),
    }
}

#[tokio::test]
async fn seeding_is_idempotent_and_never_resets_observed_applications() {
    let fixture = Fixture::new().await;
    let repo = Telemetry::new(&fixture.pool);
    let source = baseline();
    let at = source.last_applied_at.unwrap() + Duration::seconds(10);
    assert!(
        repo.get(source.corpus_id, source.document_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.record(source.corpus_id, source.document_id, Uuid::new_v4(), at)
            .await
            .unwrap()
    );
    assert!(
        !repo
            .get(source.corpus_id, source.document_id)
            .await
            .unwrap()
            .unwrap()
            .baseline_imported
    );
    repo.seed(&source).await.unwrap();
    repo.seed(&source).await.unwrap();
    let actual = repo
        .get(source.corpus_id, source.document_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.applied_count, 8);
    assert_eq!(actual.last_applied_at, Some(at));
    assert!(actual.baseline_imported);
    let mut conflict = source.clone();
    conflict.applied_count += 1;
    assert!(repo.seed(&conflict).await.is_err());
    conflict = source.clone();
    conflict.last_applied_at = None;
    assert!(repo.seed(&conflict).await.is_err());
    assert_eq!(
        repo.get(source.corpus_id, source.document_id)
            .await
            .unwrap()
            .unwrap(),
        actual
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn twenty_clients_count_distinct_events_once_and_seed_without_loss() {
    let fixture = Fixture::new().await;
    let source = baseline();
    let event = Uuid::new_v4();
    let at = source.last_applied_at.unwrap();
    let mut clients = Vec::new();
    for _ in 0..20 {
        let pool = fixture.pool.clone();
        let source = source.clone();
        clients.push(tokio::spawn(async move {
            Telemetry::new(&pool)
                .record(source.corpus_id, source.document_id, event, at)
                .await
                .unwrap()
        }));
    }
    let mut inserted = 0;
    for client in clients {
        inserted += usize::from(client.await.unwrap());
    }
    assert_eq!(inserted, 1);
    let mut clients = Vec::new();
    for index in 0..20 {
        let pool = fixture.pool.clone();
        let source = source.clone();
        clients.push(tokio::spawn(async move {
            let repo = Telemetry::new(&pool);
            let (seed, record) = tokio::join!(
                repo.seed(&source),
                repo.record(
                    source.corpus_id,
                    source.document_id,
                    Uuid::new_v4(),
                    at + Duration::seconds(index)
                )
            );
            seed.unwrap();
            assert!(record.unwrap());
        }));
    }
    for client in clients {
        client.await.unwrap();
    }
    let repo = Telemetry::new(&fixture.pool);
    assert!(
        !repo
            .record(
                source.corpus_id,
                source.document_id,
                event,
                at + Duration::days(1)
            )
            .await
            .unwrap()
    );
    let actual = repo
        .get(source.corpus_id, source.document_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.applied_count, 28);
    assert_eq!(actual.last_applied_at, Some(at + Duration::seconds(19)));
    fixture.cleanup().await;
}

#[tokio::test]
async fn document_ids_and_application_ids_are_isolated_by_corpus() {
    let fixture = Fixture::new().await;
    let repo = Telemetry::new(&fixture.pool);
    let source = baseline();
    let mut other = source.clone();
    other.corpus_id = Uuid::new_v4();
    other.applied_count = 3;
    other.last_applied_at = None;
    repo.seed(&source).await.unwrap();
    repo.seed(&other).await.unwrap();
    assert_eq!(
        repo.get(other.corpus_id, other.document_id)
            .await
            .unwrap()
            .unwrap()
            .legacy_usage()
            .unwrap(),
        other
    );
    let event = Uuid::new_v4();
    for corpus in [source.corpus_id, other.corpus_id] {
        assert!(
            repo.record(
                corpus,
                source.document_id,
                event,
                source.last_applied_at.unwrap()
            )
            .await
            .unwrap()
        );
    }
    sqlx::query("DELETE FROM knowledge_usage WHERE corpus_id = $1")
        .bind(source.corpus_id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert!(
        repo.get(source.corpus_id, source.document_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.get(other.corpus_id, other.document_id)
            .await
            .unwrap()
            .unwrap()
            .applied_count,
        4
    );
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_applications")
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(events, 1);
    fixture.cleanup().await;
}

#[tokio::test]
async fn failed_counter_update_rolls_back_event_and_retry_can_succeed() {
    let fixture = Fixture::new().await;
    let repo = Telemetry::new(&fixture.pool);
    let source = baseline();
    repo.seed(&source).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_usage() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected failure'; END $$; CREATE TRIGGER reject_usage BEFORE UPDATE ON knowledge_usage FOR EACH ROW EXECUTE FUNCTION reject_usage();")
        .execute(&fixture.pool).await.unwrap();
    let event = Uuid::new_v4();
    let at = source.last_applied_at.unwrap();
    assert!(
        repo.record(source.corpus_id, source.document_id, event, at)
            .await
            .is_err()
    );
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_applications")
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(events, 0);
    assert_eq!(
        repo.get(source.corpus_id, source.document_id)
            .await
            .unwrap()
            .unwrap()
            .applied_count,
        7
    );
    sqlx::query("DROP TRIGGER reject_usage ON knowledge_usage")
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert!(
        repo.record(source.corpus_id, source.document_id, event, at)
            .await
            .unwrap()
    );
    assert_eq!(
        repo.get(source.corpus_id, source.document_id)
            .await
            .unwrap()
            .unwrap()
            .applied_count,
        8
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn wide_totals_remain_lossless_and_legacy_conversion_never_wraps() {
    let fixture = Fixture::new().await;
    let repo = Telemetry::new(&fixture.pool);
    let mut source = baseline();
    source.applied_count = i32::MAX;
    repo.seed(&source).await.unwrap();
    repo.record(
        source.corpus_id,
        source.document_id,
        Uuid::new_v4(),
        source.last_applied_at.unwrap(),
    )
    .await
    .unwrap();
    let actual = repo
        .get(source.corpus_id, source.document_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.applied_count, i64::from(i32::MAX) + 1);
    assert!(actual.legacy_usage().is_err());
    fixture.cleanup().await;
}
