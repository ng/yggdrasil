//! Exercise the real guard DDL in an isolated schema, without switching the
//! public test corpus or assuming the operator database can be modified.
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

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
        let schema = format!("guard_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |conn, _| {
                let query = format!("SET search_path TO {path}, public");
                Box::pin(async move {
                    sqlx::query(&query).execute(conn).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE memories (id INTEGER); CREATE TABLE learnings (id INTEGER);")
            .execute(&pool)
            .await
            .unwrap();
        // Explicit public references are essential in production (a caller's
        // search_path must not substitute its own permissive marker/functions).
        let migration = include_str!("../migrations/20261008000002_knowledge_storage_guard.sql")
            .replace("public.", &format!("{schema}."));
        sqlx::raw_sql(&migration).execute(&pool).await.unwrap();
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
    async fn phase(&self, phase: &str, corpus: Option<Uuid>) {
        sqlx::query(
            "UPDATE knowledge_storage SET generation=generation+1, backend=$1, corpus_id=$2",
        )
        .bind(phase)
        .bind(corpus)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn fence_blocks_old_writers_but_keeps_sql_reads_until_okf_switch() {
    let f = Fixture::new().await;
    sqlx::query("INSERT INTO memories VALUES (1)")
        .execute(&f.pool)
        .await
        .unwrap();
    let corpus = Uuid::new_v4();
    f.phase("fenced", Some(corpus)).await;
    sqlx::query("SELECT ygg_knowledge_guard(false, 1, 2)")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("SELECT ygg_knowledge_guard(true, 1, 2)")
            .execute(&f.pool)
            .await
            .is_err()
    );
    for query in [
        "INSERT INTO memories VALUES (2)",
        "UPDATE memories SET id=2",
        "DELETE FROM memories",
        "TRUNCATE memories",
        "INSERT INTO learnings VALUES (1)",
        "UPDATE learnings SET id=2",
        "DELETE FROM learnings",
        "TRUNCATE learnings",
    ] {
        assert!(
            sqlx::query(query).execute(&f.pool).await.is_err(),
            "{query}"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM memories")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    f.phase("okf", Some(corpus)).await;
    assert!(
        sqlx::query("SELECT ygg_knowledge_guard(false, 1, 3)")
            .execute(&f.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE knowledge_storage SET generation=generation+1, backend='sql', corpus_id=NULL"
        )
        .execute(&f.pool)
        .await
        .is_err()
    );
    f.cleanup().await;
}

#[tokio::test]
async fn marker_requires_next_generation_and_rejects_old_protocol_or_epoch() {
    let f = Fixture::new().await;
    for query in [
        "DELETE FROM knowledge_storage",
        "TRUNCATE knowledge_storage",
        "UPDATE knowledge_storage SET generation=1",
        "UPDATE knowledge_storage SET generation=3",
        "UPDATE knowledge_storage SET generation=2, database_id=gen_random_uuid()",
    ] {
        assert!(
            sqlx::query(query).execute(&f.pool).await.is_err(),
            "{query}"
        );
    }
    assert!(
        sqlx::query("SELECT ygg_knowledge_guard(false, 1, 0)")
            .execute(&f.pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE knowledge_storage SET generation=2, minimum_client=2")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("SELECT ygg_knowledge_guard(false, 1, 2)")
            .execute(&f.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SELECT ygg_knowledge_guard(false, 2, 1)")
            .execute(&f.pool)
            .await
            .is_err()
    );
    sqlx::query("SELECT ygg_knowledge_guard(false, 2, 2)")
        .execute(&f.pool)
        .await
        .unwrap();
    f.cleanup().await;
}

#[tokio::test]
async fn transition_waits_for_guarded_reads_and_unaware_writer_transactions() {
    let f = Fixture::new().await;
    for guarded_read in [true, false] {
        let mut transaction = f.pool.begin().await.unwrap();
        if guarded_read {
            sqlx::query("SELECT ygg_knowledge_guard(false, 1, NULL)")
                .execute(&mut *transaction)
                .await
                .unwrap();
        } else {
            sqlx::query("INSERT INTO memories VALUES (1)")
                .execute(&mut *transaction)
                .await
                .unwrap();
        }
        let pool = f.pool.clone();
        let corpus = Uuid::new_v4();
        let mut pending = tokio::spawn(async move {
            sqlx::query("UPDATE knowledge_storage SET generation=generation+1, backend='fenced', corpus_id=$1")
                .bind(corpus).execute(&pool).await.unwrap();
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut pending)
                .await
                .is_err()
        );
        transaction.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap();
        f.phase("sql", None).await;
    }
    f.cleanup().await;
}

#[tokio::test]
async fn old_repeatable_read_snapshot_cannot_bypass_committed_fence() {
    let f = Fixture::new().await;
    let mut old = f.pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *old)
        .await
        .unwrap();
    sqlx::query("SELECT * FROM knowledge_storage")
        .fetch_all(&mut *old)
        .await
        .unwrap();
    f.phase("fenced", Some(Uuid::new_v4())).await;
    assert!(
        sqlx::query("INSERT INTO memories VALUES (1)")
            .execute(&mut *old)
            .await
            .is_err()
    );
    old.rollback().await.unwrap();
    f.cleanup().await;
}

#[tokio::test]
async fn read_only_role_can_check_guard_but_cannot_change_marker() {
    let f = Fixture::new().await;
    let mut read = f.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL ROLE pg_read_all_data")
        .execute(&mut *read)
        .await
        .unwrap();
    sqlx::query("SELECT ygg_knowledge_guard(false, 1, 1)")
        .execute(&mut *read)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE knowledge_storage SET generation=generation+1")
            .execute(&mut *read)
            .await
            .is_err()
    );
    read.rollback().await.unwrap();
    f.cleanup().await;
}
