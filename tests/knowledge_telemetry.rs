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

#[tokio::test]
async fn emitted_batches_are_idempotent_and_do_not_invent_imported_baselines() {
    use ygg::knowledge::telemetry::{Application, record_batch};
    let f = Fixture::new().await;
    let corpus = Uuid::new_v4();
    let applications: Vec<_> = [false, true]
        .into_iter()
        .map(|imported| Application {
            document: Uuid::new_v4(),
            application: Uuid::new_v4(),
            at: Utc::now(),
            imported,
        })
        .collect();
    for _ in 0..2 {
        let mut transaction = f.pool.begin().await.unwrap();
        let cache = record_batch(&mut transaction, corpus, &applications)
            .await
            .unwrap();
        assert_eq!(
            cache.len(),
            1,
            "unseeded imported observations cannot stand in for total usage"
        );
        assert_eq!(cache[0].applied_count, 1);
        transaction.commit().await.unwrap();
    }
    let repo = Telemetry::new(&f.pool);
    repo.seed(&Usage {
        corpus_id: corpus,
        document_id: applications[1].document,
        applied_count: 42,
        last_applied_at: None,
    })
    .await
    .unwrap();
    let mut transaction = f.pool.begin().await.unwrap();
    let cache = record_batch(&mut transaction, corpus, &applications)
        .await
        .unwrap();
    assert_eq!(cache.len(), 2);
    assert_eq!(
        cache
            .iter()
            .find(|t| t.document_id == applications[1].document)
            .unwrap()
            .applied_count,
        43
    );
    transaction.commit().await.unwrap();
    let pending: Vec<_> = applications
        .iter()
        .cloned()
        .map(|mut a| {
            a.application = Uuid::new_v4();
            a
        })
        .collect();
    let mut transaction = f.pool.begin().await.unwrap();
    record_batch(&mut transaction, corpus, &pending)
        .await
        .unwrap();
    transaction.rollback().await.unwrap();
    assert_eq!(
        repo.get(corpus, applications[0].document)
            .await
            .unwrap()
            .unwrap()
            .applied_count,
        1
    );
    assert_eq!(
        repo.get(corpus, applications[1].document)
            .await
            .unwrap()
            .unwrap()
            .applied_count,
        43
    );
    let mut workers = Vec::new();
    for index in 0..20 {
        let pool = f.pool.clone();
        let mut batch = applications.clone();
        for a in &mut batch {
            a.application = Uuid::new_v4();
        }
        if index % 2 == 0 {
            batch.reverse();
        }
        workers.push(tokio::spawn(async move {
            let mut transaction = pool.begin().await.unwrap();
            record_batch(&mut transaction, corpus, &batch)
                .await
                .unwrap();
            transaction.commit().await.unwrap();
        }));
    }
    for worker in workers {
        worker.await.unwrap();
    }
    assert_eq!(
        repo.get(corpus, applications[0].document)
            .await
            .unwrap()
            .unwrap()
            .applied_count,
        21
    );
    assert_eq!(
        repo.get(corpus, applications[1].document)
            .await
            .unwrap()
            .unwrap()
            .applied_count,
        63
    );
    f.cleanup().await;
}

#[tokio::test]
async fn batch_preserves_intermediate_totals_first_timestamps_and_atomic_overflow() {
    use ygg::knowledge::telemetry::{Application, record_batch};
    let f = Fixture::new().await;
    let corpus = Uuid::new_v4();
    let document = Uuid::new_v4();
    let at = Utc.with_ymd_and_hms(2026, 10, 9, 0, 0, 0).unwrap();
    let first = Application {
        document,
        application: Uuid::from_u128(1),
        at,
        imported: false,
    };
    let mut duplicate = first.clone();
    duplicate.at = at + Duration::days(1);
    let second = Application {
        application: Uuid::from_u128(2),
        at: at + Duration::seconds(1),
        ..first.clone()
    };
    let batch = [second.clone(), first.clone(), duplicate];
    let mut tx = f.pool.begin().await.unwrap();
    assert!(record_batch(&mut tx, corpus, &[]).await.unwrap().is_empty());
    let totals = record_batch(&mut tx, corpus, &batch).await.unwrap();
    assert_eq!(
        totals.iter().map(|t| t.applied_count).collect::<Vec<_>>(),
        [1, 1, 2]
    );
    assert_eq!(totals[0].last_applied_at, Some(at));
    assert_eq!(totals[1].last_applied_at, Some(at));
    assert_eq!(totals[2].last_applied_at, Some(second.at));
    tx.commit().await.unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    let totals = record_batch(&mut tx, corpus, &batch).await.unwrap();
    assert!(
        totals
            .iter()
            .all(|t| t.applied_count == 2 && t.last_applied_at == Some(second.at))
    );
    tx.commit().await.unwrap();
    sqlx::query(
        "UPDATE knowledge_usage SET observed_count=$1 WHERE corpus_id=$2 AND document_id=$3",
    )
    .bind(i64::MAX)
    .bind(corpus)
    .bind(document)
    .execute(&f.pool)
    .await
    .unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    let overflow = Application {
        application: Uuid::new_v4(),
        ..first
    };
    assert!(
        record_batch(&mut tx, corpus, &[overflow.clone()])
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM knowledge_applications WHERE corpus_id=$1 AND application_id=$2",
    )
    .bind(corpus)
    .bind(overflow.application)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    // The observed column can fit while imported + observed overflows. Force
    // the SQL aggregate expression to be evaluated and abort that transaction.
    sqlx::query("UPDATE knowledge_usage SET imported_count=1, observed_count=$1 WHERE corpus_id=$2 AND document_id=$3")
        .bind(i64::MAX - 1).bind(corpus).bind(document).execute(&f.pool).await.unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        record_batch(&mut tx, corpus, &[overflow.clone()])
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM knowledge_applications WHERE corpus_id=$1 AND application_id=$2",
    )
    .bind(corpus)
    .bind(overflow.application)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        Telemetry::new(&f.pool)
            .get(corpus, document)
            .await
            .unwrap()
            .unwrap()
            .applied_count,
        i64::MAX
    );
    f.cleanup().await;
}

#[tokio::test]
async fn batches_and_single_writers_order_mixed_existing_and_missing_rows() {
    use ygg::knowledge::telemetry::{Application, record_batch};
    let f = Fixture::new().await;
    let corpus = Uuid::new_v4();
    let mut docs = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    docs.sort();
    // Middle row exists; acquiring missing rows separately would invert locks.
    Telemetry::new(&f.pool)
        .seed(&Usage {
            corpus_id: corpus,
            document_id: docs[1],
            applied_count: 7,
            last_applied_at: None,
        })
        .await
        .unwrap();
    let at = Utc::now();
    let apps: Vec<_> = docs
        .iter()
        .map(|&document| Application {
            document,
            application: Uuid::new_v4(),
            at,
            imported: false,
        })
        .collect();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(20));
    let mut workers = Vec::new();
    for index in 0..20 {
        let pool = f.pool.clone();
        let mut batch = apps.clone();
        let barrier = barrier.clone();
        if index % 2 == 0 {
            batch.reverse();
        }
        workers.push(tokio::spawn(async move {
            barrier.wait().await;
            if index % 3 == 0 {
                let a = &batch[1];
                Telemetry::new(&pool)
                    .record(corpus, a.document, a.application, a.at)
                    .await
                    .unwrap();
            } else {
                let mut tx = pool.begin().await.unwrap();
                record_batch(&mut tx, corpus, &batch).await.unwrap();
                tx.commit().await.unwrap();
            }
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        for worker in workers {
            worker.await.unwrap();
        }
    })
    .await
    .expect("mixed writers must not deadlock");
    for (index, document) in docs.into_iter().enumerate() {
        let total = Telemetry::new(&f.pool)
            .get(corpus, document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(total.applied_count, if index == 1 { 8 } else { 1 });
    }
    f.cleanup().await;
}
