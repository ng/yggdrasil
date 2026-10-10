#![cfg(any(target_os = "macos", target_os = "linux"))]
use futures::FutureExt;
use sqlx::{Connection, postgres::PgConnectOptions};
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    time::Duration,
};
use ygg::db::{runtime::ManagedCluster, supervisor};

#[tokio::test]
#[ignore = "requires native YGG_TEST_PG_BIN (16) and YGG_TEST_PG18_ARCHIVE (pinned 18.6)"]
async fn managed_major_restore_uses_new_directory_and_preserves_source_and_claims() {
    let bin = PathBuf::from(std::env::var("YGG_TEST_PG_BIN").unwrap());
    let archive = PathBuf::from(std::env::var("YGG_TEST_PG18_ARCHIVE").unwrap());
    let temp = tempfile::Builder::new()
        .prefix("ymajor-cli-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    let source_data = base.join("source");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&source_data)
        .unwrap();
    let target_data = base.join("target");
    let source = ManagedCluster::initialize(&source_data.join("postgres"), &bin, 16)
        .await
        .unwrap();
    let mut owner = Some(source.try_owner().unwrap().unwrap());
    let options = |cluster: &ManagedCluster| {
        PgConnectOptions::new()
            .host(cluster.socket_dir().to_str().unwrap())
            .port(5432)
            .username("ygg_owner")
            .database("ygg")
    };
    let command = |target: bool| {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
        cmd.env_remove("DATABASE_URL")
            .env_remove("YGG_DATABASE_OWNER_URL")
            .env_remove("YGG_PROFILE")
            .env("YGG_DB_MODE", "managed")
            .env(
                "YGG_DATA_DIR",
                if target { &target_data } else { &source_data },
            )
            .env("YGG_CONFIG_DIR", base.join("config"))
            .env("YGG_KNOWLEDGE_DIR", base.join("knowledge"))
            .env("YGG_KNOWLEDGE_POLICY_DIR", base.join("policy"));
        cmd
    };
    let result = std::panic::AssertUnwindSafe(async {
        owner.as_mut().unwrap().start_or_adopt(Duration::from_secs(30)).await.unwrap();
        ygg::db::provision::migrate(&source).await.unwrap();
        drop(owner.take());
        let mut connection = sqlx::PgConnection::connect_with(&options(&source)).await.unwrap();
        let agent: uuid::Uuid = sqlx::query_scalar("INSERT INTO agents(agent_name) VALUES ('major-cli') RETURNING agent_id").fetch_one(&mut connection).await.unwrap();
        let repo: uuid::Uuid = sqlx::query_scalar("INSERT INTO repos(name,task_prefix) VALUES ('major-cli','majorcli') RETURNING repo_id").fetch_one(&mut connection).await.unwrap();
        let task: uuid::Uuid = sqlx::query_scalar("INSERT INTO tasks(repo_id,seq,title,status,assignee) VALUES ($1,1,'preserved λ','in_progress',$2) RETURNING task_id").bind(repo).bind(agent).fetch_one(&mut connection).await.unwrap();
        let run: uuid::Uuid = sqlx::query_scalar("INSERT INTO task_runs(task_id,attempt,idempotency_key,state,agent_id,claimed_at) VALUES ($1,1,'major-cli-claim','running',$2,now()) RETURNING run_id").bind(task).bind(agent).fetch_one(&mut connection).await.unwrap();
        sqlx::query("UPDATE tasks SET current_attempt_id=$1 WHERE task_id=$2").bind(run).bind(task).execute(&mut connection).await.unwrap();
        let database: uuid::Uuid = sqlx::query_scalar("SELECT database_id FROM knowledge_storage WHERE singleton").fetch_one(&mut connection).await.unwrap();
        connection.close().await.unwrap();
        let store = ygg::knowledge::store::KnowledgeStore::open(&base.join("knowledge"), true).unwrap();
        let document = ygg::knowledge::document::Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap();
        let saved = store.put(&document, ygg::knowledge::store::ExpectedRevision::Absent).unwrap();
        ygg::knowledge::identity::IdentityRegistry::open(&base.join("policy"), true).unwrap().initialize(true).unwrap();
        let backup = base.join("backup");
        let output = command(false).args(["db","backup"]).arg(&backup).arg("--json").output().await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let files = base.join("files");
        let restore = |major: &str| {
            let mut cmd = command(true);
            cmd.args(["db","restore"]).arg(&backup).arg("--destination").arg(&files)
                .arg("--postgres-major").arg(major).arg("--postgres-archive").arg(&archive).arg("--json");
            cmd
        };
        let unsupported = restore("17").output().await.unwrap();
        assert!(!unsupported.status.success());
        assert!(!target_data.exists() && !files.exists());
        let wrong_major = restore("16").output().await.unwrap();
        assert!(!wrong_major.status.success());
        assert!(!target_data.exists() && !files.exists());
        let output = restore("18").output().await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["database_id"], database.to_string());
        assert_eq!(receipt["configuration_switched"], false);
        assert!(receipt["corpus_id"].is_string());
        let restored_store = ygg::knowledge::store::KnowledgeStore::open(&files.join("knowledge"), false).unwrap();
        assert_eq!(restored_store.get(saved.key).unwrap().unwrap().revision, saved.revision);
        assert_eq!(store.get(saved.key).unwrap().unwrap().revision, saved.revision);
        assert!(!base.join("config/config.toml").exists());
        let selected = ManagedCluster::open(&target_data.join("postgres")).unwrap();
        assert_ne!(selected.id(), source.id());
        assert_eq!(std::fs::read_to_string(target_data.join("postgres/data/PG_VERSION")).unwrap().trim(), "18");
        assert_eq!(std::fs::read_to_string(source_data.join("postgres/data/PG_VERSION")).unwrap().trim(), "16");
        let output = command(true).args(["db","start"]).output().await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        for cluster in [&source, &selected] {
            let mut connection = sqlx::PgConnection::connect_with(&options(cluster)).await.unwrap();
            let restored: (uuid::Uuid, uuid::Uuid, String) = sqlx::query_as("SELECT assignee,current_attempt_id,title FROM tasks WHERE task_id=$1").bind(task).fetch_one(&mut connection).await.unwrap();
            assert_eq!(restored, (agent,run,"preserved λ".into()));
            let current: uuid::Uuid = sqlx::query_scalar("SELECT database_id FROM knowledge_storage WHERE singleton").fetch_one(&mut connection).await.unwrap();
            assert_eq!(current,database);
            connection.close().await.unwrap();
        }
        let repeated = restore("18").output().await.unwrap();
        assert!(!repeated.status.success(), "existing target must never be overwritten");
        // Complete the explicit dump/restore/configuration-switch workflow,
        // then prove that a confirmed switch retry cannot replay old rows.
        let switch_config = base.join("switch-config");
        std::fs::DirBuilder::new().mode(0o700).create(&switch_config).unwrap();
        let proposed = base.join("proposed.toml");
        let mut settings = toml::Table::new();
        for (key, path) in [("data_dir", target_data.clone()), ("knowledge_dir", files.join("knowledge")), ("knowledge_policy_dir", files.join("policy"))] {
            settings.insert(key.into(), toml::Value::String(path.to_str().unwrap().into()));
        }
        settings.insert("database".into(), toml::Value::Table(toml::Table::from_iter([("mode".into(), toml::Value::String("managed".into()))])));
        std::fs::write(&proposed, toml::to_string(&settings).unwrap()).unwrap();
        std::fs::set_permissions(&proposed, std::fs::Permissions::from_mode(0o600)).unwrap();
        let switching = || {
            let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
            for key in ["DATABASE_URL", "YGG_DATABASE_OWNER_URL", "YGG_DB_MODE", "YGG_DATA_DIR", "YGG_PROFILE", "YGG_KNOWLEDGE_DIR", "YGG_KNOWLEDGE_POLICY_DIR"] { cmd.env_remove(key); }
            cmd.env("YGG_CONFIG_DIR", &switch_config).args(["db", "switch"]).arg(&backup)
                .arg("--restore-dir").arg(&files).arg("--target-config").arg(&proposed).arg("--json");
            cmd
        };
        let switched = switching().output().await.unwrap();
        assert!(switched.status.success(), "{}", String::from_utf8_lossy(&switched.stderr));
        let outcome: serde_json::Value = serde_json::from_slice(&switched.stdout).unwrap();
        let selected_config = ygg::config::database::DeploymentConfig::load(std::collections::BTreeMap::from([("YGG_CONFIG_DIR".into(), switch_config.to_str().unwrap().into())])).unwrap();
        assert!(matches!(selected_config.database, ygg::config::database::DatabaseTarget::ManagedLocal { data_dir } if data_dir == target_data));
        assert_eq!(selected_config.knowledge_dir, files.join("knowledge"));
        let mut connection = sqlx::PgConnection::connect_with(&options(&selected)).await.unwrap();
        let version: String = sqlx::query_scalar("SHOW server_version").fetch_one(&mut connection).await.unwrap();
        assert!(version.starts_with("18.6"));
        sqlx::query("UPDATE tasks SET title='after major switch' WHERE task_id=$1").bind(task).execute(&mut connection).await.unwrap();
        let resumed = switching().arg("--resume").arg(outcome["operation"].as_str().unwrap()).output().await.unwrap();
        assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
        let title: String = sqlx::query_scalar("SELECT title FROM tasks WHERE task_id=$1").bind(task).fetch_one(&mut connection).await.unwrap();
        assert_eq!(title, "after major switch");
        connection.close().await.unwrap();
    }).catch_unwind().await;
    drop(owner);
    for root in [source_data.join("postgres"), target_data.join("postgres")] {
        if root.join("cluster.json").exists() {
            let cluster = ManagedCluster::open(&root).unwrap();
            if let Err(error) = supervisor::stop(&cluster, Duration::from_secs(30)).await {
                let retained = temp.keep();
                panic!("retained fixture {retained:?}: {error:#}");
            }
        }
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
