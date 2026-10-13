#![cfg(unix)]
use futures::FutureExt;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{os::unix::fs::DirBuilderExt, path::PathBuf, time::Duration};
use ygg::db::{runtime::ManagedCluster, supervisor};

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_OLD_BIN (16.14) and YGG_TEST_PG_ARCHIVE (pinned 16.15)"]
async fn explicit_patch_upgrade_preserves_rows_backup_identity_and_later_writes() {
    let old_bin = PathBuf::from(std::env::var("YGG_TEST_PG_OLD_BIN").unwrap());
    let archive = PathBuf::from(std::env::var("YGG_TEST_PG_ARCHIVE").unwrap());
    let temp = tempfile::Builder::new()
        .prefix("yupgrade-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    let data = base.join("data");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&data)
        .unwrap();
    let root = data.join("postgres");
    let source = ManagedCluster::initialize(&root, &old_bin, 16)
        .await
        .unwrap();
    let mut owner = source.try_owner().unwrap().unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        owner.start_or_adopt(Duration::from_secs(30)).await.unwrap();
        ygg::db::provision::migrate(&source).await.unwrap();
        drop(owner);
        let options = PgConnectOptions::new().host(source.socket_dir().to_str().unwrap()).port(5432)
            .username("ygg_owner").database("ygg");
        let pool = PgPoolOptions::new().max_connections(1).connect_with(options.clone()).await.unwrap();
        let repo: uuid::Uuid = sqlx::query_scalar("INSERT INTO repos(name, task_prefix) VALUES ('patch-upgrade', 'patch') RETURNING repo_id").fetch_one(&pool).await.unwrap();
        let task: uuid::Uuid = sqlx::query_scalar("INSERT INTO tasks(repo_id, seq, title) VALUES ($1, 1, 'preserved λ') RETURNING task_id").bind(repo).fetch_one(&pool).await.unwrap();
        let database: uuid::Uuid = sqlx::query_scalar("SELECT database_id FROM knowledge_storage WHERE singleton").fetch_one(&pool).await.unwrap();
        let old_version: String = sqlx::query_scalar("SHOW server_version").fetch_one(&pool).await.unwrap();
        assert!(old_version.starts_with("16.14"));
        pool.close().await;
        let knowledge = base.join("knowledge");
        let policy = base.join("policy");
        let store = ygg::knowledge::store::KnowledgeStore::open(&knowledge, true).unwrap();
        let document = ygg::knowledge::document::Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap();
        let saved = store.put(&document, ygg::knowledge::store::ExpectedRevision::Absent).unwrap();
        ygg::knowledge::identity::IdentityRegistry::open(&policy, true).unwrap().initialize(true).unwrap();
        let backup = base.join("backup");
        let command = || {
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
            command.env_remove("DATABASE_URL").env_remove("YGG_DATABASE_OWNER_URL")
                .env("YGG_DB_MODE", "managed").env("YGG_DATA_DIR", &data).env("YGG_CONFIG_DIR", base.join("config"))
                .env("YGG_KNOWLEDGE_DIR", &knowledge).env("YGG_KNOWLEDGE_POLICY_DIR", &policy)
                .args(["db", "upgrade", "--backup"]).arg(&backup).arg("--postgres-archive").arg(&archive).arg("--json");
            command
        };
        let refused = command().output().await.unwrap();
        assert!(!refused.status.success());
        assert!(!root.join("upgrade.json").exists());
        assert!(!backup.exists());
        // A real persistent old owner must relinquish its lifetime lease.
        supervisor::start(&source, std::path::Path::new(env!("CARGO_BIN_EXE_ygg")), Duration::from_secs(30)).await.unwrap();
        let output = command().arg("--quiesced").output().await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["complete"], true);
        assert_eq!(receipt["cluster_id"], source.id().to_string());
        let selected = ManagedCluster::open(&root).unwrap();
        assert_eq!(selected.id(), source.id());
        assert!(old_bin.join("postgres").is_file());
        let backup_manifest = ygg::db::deployment_backup::verify(&backup).unwrap();
        assert_eq!(backup_manifest.database.database_id, database);
        assert!(backup_manifest.knowledge.is_some());
        let retained = store.get(saved.key).unwrap().unwrap();
        assert_eq!(retained.revision, saved.revision);
        let pool = PgPoolOptions::new().max_connections(1).connect_with(options.clone()).await.unwrap();
        let version: String = sqlx::query_scalar("SHOW server_version").fetch_one(&pool).await.unwrap();
        assert!(version.starts_with("16.15"));
        let title: String = sqlx::query_scalar("SELECT title FROM tasks WHERE task_id=$1").bind(task).fetch_one(&pool).await.unwrap();
        assert_eq!(title, "preserved λ");
        sqlx::query("UPDATE tasks SET title='after completion' WHERE task_id=$1").bind(task).execute(&pool).await.unwrap();
        pool.close().await;
        let resumed = command().arg("--quiesced").arg("--resume").arg(receipt["operation"].as_str().unwrap()).output().await.unwrap();
        assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
        let pool = PgPoolOptions::new().max_connections(1).connect_with(options).await.unwrap();
        let title: String = sqlx::query_scalar("SELECT title FROM tasks WHERE task_id=$1").bind(task).fetch_one(&pool).await.unwrap();
        assert_eq!(title, "after completion");
        pool.close().await;
    }).catch_unwind().await;
    let selected = ManagedCluster::open(&root).unwrap_or(source);
    if let Err(error) = supervisor::stop(&selected, Duration::from_secs(30)).await {
        let retained = temp.keep();
        panic!("upgrade fixture retained at {retained:?}: {error:#}");
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
