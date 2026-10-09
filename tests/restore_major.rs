#![cfg(unix)]
use futures::FutureExt;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};
use ygg::db::{backup, restore, runtime::ManagedCluster};

#[tokio::test]
#[ignore = "requires native YGG_TEST_PG16_BIN and YGG_TEST_PG18_BIN; starts two disposable clusters"]
async fn pg16_to_pg18_preserves_claims_rows_and_strict_constraints() {
    let source_bin = PathBuf::from(std::env::var("YGG_TEST_PG16_BIN").unwrap());
    let target_bin = PathBuf::from(std::env::var("YGG_TEST_PG18_BIN").unwrap());
    let temp = tempfile::Builder::new()
        .prefix("ymajor-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = ManagedCluster::initialize(&root.join("source"), &source_bin, 16)
        .await
        .unwrap();
    let target = ManagedCluster::initialize(&root.join("target"), &target_bin, 18)
        .await
        .unwrap();
    let mut source_owner = source.try_owner().unwrap().unwrap();
    let mut target_owner = target.try_owner().unwrap().unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        source_owner.start_or_adopt(Duration::from_secs(30)).await.unwrap();
        target_owner.start_or_adopt(Duration::from_secs(30)).await.unwrap();
        let options = |cluster: &ManagedCluster| PgConnectOptions::new()
            .host(cluster.socket_dir().to_str().unwrap()).port(5432)
            .username("ygg_bootstrap").database("postgres");
        let source_options = options(&source);
        let target_options = options(&target);
        let pool = PgPoolOptions::new().max_connections(2).connect_with(source_options.clone()).await.unwrap();
        ygg::db::run_migrations(&pool).await.unwrap();
        let agent: uuid::Uuid = sqlx::query_scalar("INSERT INTO agents(agent_name) VALUES ('major-restore-owner') RETURNING agent_id").fetch_one(&pool).await.unwrap();
        let repo: uuid::Uuid = sqlx::query_scalar("INSERT INTO repos(name, task_prefix) VALUES ('major-restore', 'major') RETURNING repo_id").fetch_one(&pool).await.unwrap();
        let task: uuid::Uuid = sqlx::query_scalar("INSERT INTO tasks(repo_id, seq, title, status, assignee) VALUES ($1, 1, 'claimed λ task', 'in_progress', $2) RETURNING task_id").bind(repo).bind(agent).fetch_one(&pool).await.unwrap();
        let run: uuid::Uuid = sqlx::query_scalar("INSERT INTO task_runs(task_id, attempt, idempotency_key, state, agent_id, claimed_at) VALUES ($1, 1, 'major-restore-claim', 'running', $2, now()) RETURNING run_id").bind(task).bind(agent).fetch_one(&pool).await.unwrap();
        sqlx::query("UPDATE tasks SET current_attempt_id=$1 WHERE task_id=$2").bind(run).bind(task).execute(&pool).await.unwrap();
        sqlx::raw_sql("CREATE DOMAIN positive AS int CONSTRAINT positive_check CHECK (VALUE > 0);
            CREATE TABLE restore_probe (id uuid PRIMARY KEY, body text NOT NULL, metadata jsonb, labels text[], amount positive, CONSTRAINT body_check CHECK (body <> ''));
            INSERT INTO restore_probe VALUES (gen_random_uuid(), 'preserved λ', '{\"unknown\": [1, null]}', ARRAY['a', 'λ'], 1), (gen_random_uuid(), 'nullable', NULL, NULL, NULL)")
            .execute(&pool).await.unwrap();
        let archive_path = root.join("database.dump");
        let mut archive = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&archive_path).unwrap();
        let snapshot = backup::dump(&source_bin, &source_options, &mut archive).await.unwrap();
        assert_eq!(snapshot.server_major, 16);
        assert_eq!(snapshot.table_rows["\"public\".\"restore_probe\""], 2);
        pool.close().await;
        restore::database(&target_bin, &target_options, &mut archive, &snapshot).await.unwrap();
        restore::validate(&source_options, &snapshot).await.unwrap();
        restore::validate(&target_options, &snapshot).await.unwrap();
        let pool = PgPoolOptions::new().max_connections(1).connect_with(target_options.clone()).await.unwrap();
        let restored: (uuid::Uuid, uuid::Uuid, String, String) = sqlx::query_as("SELECT t.assignee, t.current_attempt_id, r.state::text, r.idempotency_key FROM tasks t JOIN task_runs r ON r.run_id=t.current_attempt_id WHERE t.task_id=$1")
            .bind(task).fetch_one(&pool).await.unwrap();
        assert_eq!(restored, (agent, run, "running".into(), "major-restore-claim".into()));
        // Each committed mutation must fail validation. Undo only our explicit
        // fixture mutation; never clean/replay a failed restore target.
        for (change, undo) in [
            ("ALTER TABLE restore_probe DROP CONSTRAINT body_check", "ALTER TABLE restore_probe ADD CONSTRAINT body_check CHECK (body <> '')"),
            ("ALTER DOMAIN positive DROP CONSTRAINT positive_check", "ALTER DOMAIN positive ADD CONSTRAINT positive_check CHECK (VALUE > 0)"),
            ("ALTER TABLE restore_probe ALTER COLUMN body DROP NOT NULL", "ALTER TABLE restore_probe ALTER COLUMN body SET NOT NULL"),
            ("UPDATE restore_probe SET body='modified' WHERE body='nullable'", "UPDATE restore_probe SET body='nullable' WHERE body='modified'"),
        ] {
            sqlx::query(change).execute(&pool).await.unwrap();
            assert!(restore::validate(&target_options, &snapshot).await.is_err(), "accepted {change}");
            sqlx::query(undo).execute(&pool).await.unwrap();
            restore::validate(&target_options, &snapshot).await.unwrap();
        }
        pool.close().await;
    }).catch_unwind().await;
    let stopped_source = source_owner.stop(Duration::from_secs(30)).await;
    let stopped_target = target_owner.stop(Duration::from_secs(30)).await;
    if stopped_source.is_err() || stopped_target.is_err() {
        let retained = temp.keep();
        panic!(
            "retained cluster fixture {retained:?}: source={stopped_source:?}, target={stopped_target:?}"
        );
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
