#![cfg(any(target_os = "macos", target_os = "linux"))]
use futures::FutureExt;
use sqlx::{postgres::PgConnectOptions, postgres::PgPoolOptions};
use std::{path::PathBuf, time::Duration};
use ygg::db::{package, runtime::ManagedCluster};

#[test]
fn manifest_and_checksum_failures_never_create_an_installation() {
    let manifest = package::release().unwrap();
    assert_eq!(manifest.postgres_version, "16.15");
    assert_eq!(manifest.source_commit.len(), 40);
    for target in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
    ] {
        let p = serde_json::to_value(package::package(target).unwrap()).unwrap();
        assert_eq!(p["sha256"].as_str().unwrap().len(), 64);
        assert!(p["url"].as_str().unwrap().contains("/16.15.0/"));
    }
    assert!(package::package("aarch64-unknown-linux-gnu").is_err());
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("bad.tar.gz");
    let destination = temp.path().join("binaries");
    std::fs::write(&archive, b"not an archive").unwrap();
    assert!(package::install_offline(&destination, &archive).is_err());
    assert!(!destination.exists());
    let native =
        serde_json::to_value(package::package(package::current_target().unwrap()).unwrap())
            .unwrap();
    let file = std::fs::File::create(&archive).unwrap();
    file.set_len(native["bytes"].as_u64().unwrap()).unwrap();
    assert!(
        package::install_offline(&destination, &archive)
            .unwrap_err()
            .to_string()
            .contains("SHA-256")
    );
    assert!(!destination.exists());
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_ARCHIVE pointing to the exact native pinned archive"]
async fn pinned_offline_package_runs_migrations_and_refuses_modified_installation() {
    let archive = PathBuf::from(std::env::var("YGG_TEST_PG_ARCHIVE").unwrap());
    let temp = tempfile::Builder::new()
        .prefix("ypkg-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap().join("binaries");
    let bin = package::install_offline(&base, &archive).unwrap();
    assert_eq!(package::install_offline(&base, &archive).unwrap(), bin);
    if std::env::var_os("YGG_TEST_PG_DOWNLOAD").is_some() {
        assert_eq!(package::install_download(&base).await.unwrap(), bin);
    }
    assert!(
        bin.parent()
            .unwrap()
            .join("YGG-THIRD-PARTY-NOTICES.txt")
            .is_file()
    );
    let root = temp.path().canonicalize().unwrap().join("cluster");
    let cluster = ManagedCluster::initialize(&root, &bin, 16).await.unwrap();
    let mut owner = cluster.try_owner().unwrap().unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        owner.start_or_adopt(Duration::from_secs(30)).await.unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(
                PgConnectOptions::new()
                    .host(root.join("runtime").to_str().unwrap())
                    .port(5432)
                    .username("ygg_bootstrap")
                    .database("postgres"),
            )
            .await
            .unwrap();
        ygg::db::run_migrations(&pool).await.unwrap();
        let uuid: uuid::Uuid = sqlx::query_scalar("SELECT uuid_generate_v4()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!uuid.is_nil());
        let version: String = sqlx::query_scalar("SHOW server_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(version.starts_with("16.15"), "{version}");
        // External init must retain the URL database (postgres here), and must
        // never manufacture a different database named ygg or managed state.
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
            .env("YGG_DB_MODE", "external")
            .env(
                "DATABASE_URL",
                format!(
                    "postgresql://ygg_bootstrap@localhost/postgres?host={}",
                    root.join("runtime").display()
                ),
            )
            .env("YGG_CONFIG_DIR", temp.path().join("external-config"))
            .env("YGG_DATA_DIR", temp.path().join("external-data"))
            .args(["init", "--yes", "--skip", "tmux,jq,rtk,hooks,project"])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = 'ygg')")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!exists);
        assert!(!temp.path().join("external-data").exists());
        // Concurrent explicit provisioning must converge on the same roles and
        // schema. Re-running must preserve database/knowledge identities.
        let (first, second) = tokio::join!(
            ygg::db::provision::migrate(&cluster),
            ygg::db::provision::migrate(&cluster)
        );
        first.unwrap();
        second.unwrap();
        let diagnostic =
            ygg::db::diagnostics::inspect(&ygg::db::provision::runtime_options(&cluster))
                .await
                .unwrap();
        assert!(diagnostic.uuid_ossp_installed);
        assert!(diagnostic.tested_major);
        assert!(!diagnostic.runtime_superuser);
        assert!(!diagnostic.runtime_create_role);
        assert!(!diagnostic.runtime_create_database);
        assert!(!diagnostic.runtime_create_schema);
        assert!(!diagnostic.runtime_create_public_objects);

        // External roles are operator-provided. A runtime without DDL rights
        // cannot migrate; an explicit owner credential migrates the same DB.
        sqlx::query("CREATE DATABASE ygg_owner_test OWNER ygg_owner")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("REVOKE ALL ON DATABASE ygg_owner_test FROM PUBLIC")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("GRANT CONNECT ON DATABASE ygg_owner_test TO ygg_runtime")
            .execute(&pool)
            .await
            .unwrap();
        let connection_url = |user: &str| {
            format!(
                "postgresql://{user}@localhost/ygg_owner_test?host={}",
                root.join("runtime").display()
            )
        };
        let cli = || {
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
            command
                .env("DATABASE_URL", connection_url("ygg_runtime"))
                .env("YGG_DB_MODE", "external")
                .env("YGG_CONFIG_DIR", temp.path().join("role-config"))
                .env("YGG_DATA_DIR", temp.path().join("role-data"))
                .env_remove("YGG_DATABASE_OWNER_URL");
            command
        };
        assert!(
            !cli()
                .arg("migrate")
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        let migrated = cli()
            .arg("migrate")
            .env("YGG_DATABASE_OWNER_URL", connection_url("ygg_owner"))
            .output()
            .await
            .unwrap();
        assert!(
            migrated.status.success(),
            "{}",
            String::from_utf8_lossy(&migrated.stderr)
        );
        let operator = ygg::db::create_pool(&connection_url("ygg_bootstrap"))
            .await
            .unwrap();
        sqlx::query("GRANT SELECT ON ALL TABLES IN SCHEMA public TO ygg_runtime")
            .execute(&operator)
            .await
            .unwrap();
        operator.close().await;
        // Read-only checks never contact the configured owner, even when that
        // credential is unusable. Ordinary runtime connections stay limited.
        assert!(
            cli()
                .args(["migrate", "--check"])
                .env("YGG_DATABASE_OWNER_URL", connection_url("no_such_role"))
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert!(!temp.path().join("role-data").exists());

        // Dump and restore use only this disposable cluster. A held table lock
        // delays inventory after its MVCC snapshot starts, so a later committed
        // row must be absent from both the manifest count and restored archive.
        // Keep real coordination identities and an acknowledged running claim
        // in the snapshot, including the task/run cycle and idempotency key.
        let claim_agent: uuid::Uuid = sqlx::query_scalar("INSERT INTO agents(agent_name) VALUES ('restore-owner') RETURNING agent_id").fetch_one(&pool).await.unwrap();
        let claim_repo: uuid::Uuid = sqlx::query_scalar("INSERT INTO repos(name, task_prefix) VALUES ('restore-repo', 'restore') RETURNING repo_id").fetch_one(&pool).await.unwrap();
        let claim_task: uuid::Uuid = sqlx::query_scalar("INSERT INTO tasks(repo_id, seq, title, status, assignee) VALUES ($1, 1, 'claimed task', 'in_progress', $2) RETURNING task_id").bind(claim_repo).bind(claim_agent).fetch_one(&pool).await.unwrap();
        let claim_run: uuid::Uuid = sqlx::query_scalar("INSERT INTO task_runs(task_id, attempt, idempotency_key, state, agent_id, claimed_at) VALUES ($1, 1, 'restore-acknowledged-claim', 'running', $2, now()) RETURNING run_id").bind(claim_task).bind(claim_agent).fetch_one(&pool).await.unwrap();
        sqlx::query("UPDATE tasks SET current_attempt_id=$1 WHERE task_id=$2").bind(claim_run).bind(claim_task).execute(&pool).await.unwrap();
        sqlx::query("CREATE FUNCTION restore_pause() RETURNS int LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(COALESCE(NULLIF(current_setting('ygg.restore_delay', true), ''), '0')::double precision); RETURN 42; END $$").execute(&pool).await.unwrap();
        sqlx::query("CREATE MATERIALIZED VIEW restore_pause_view AS SELECT restore_pause() AS value").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE backup_probe (id uuid PRIMARY KEY, discarded text, body text NOT NULL CHECK (body <> ''))").execute(&pool).await.unwrap();
        sqlx::query("ALTER TABLE backup_probe DROP COLUMN discarded").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE zz_backup_barrier (id int)").execute(&pool).await.unwrap();
        let preserved_id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO backup_probe VALUES ($1, 'before snapshot')").bind(preserved_id).execute(&pool).await.unwrap();
        let mut barrier = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE zz_backup_barrier IN ACCESS EXCLUSIVE MODE").execute(&mut *barrier).await.unwrap();
        let source_options = PgConnectOptions::new().host(root.join("runtime").to_str().unwrap()).port(5432).username("ygg_bootstrap").database("postgres").application_name("ygg-backup-snapshot-test");
        use std::os::unix::fs::{OpenOptionsExt, DirBuilderExt, PermissionsExt};
        let archive = temp.path().join("snapshot.dump");
        let mut output = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&archive).unwrap();
        let dump_bin = bin.clone();
        let dump_options = source_options.clone();
        let dumping = tokio::spawn(async move { ygg::db::backup::dump(&dump_bin, &dump_options, &mut output).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let active: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE application_name = 'ygg-backup-snapshot-test' AND backend_xmin IS NOT NULL AND wait_event_type = 'Lock')").fetch_one(&pool).await.unwrap();
                if active { break; }
                assert!(!dumping.is_finished(), "dump exited before snapshot barrier");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap();
        sqlx::query("INSERT INTO backup_probe VALUES (gen_random_uuid(), 'after snapshot')").execute(&pool).await.unwrap();
        barrier.commit().await.unwrap();
        let receipt = dumping.await.unwrap().unwrap();
        assert_eq!(receipt.table_rows["\"public\".\"backup_probe\""], 1);
        assert_eq!(receipt.sha256.len(), 64);
        assert_eq!(receipt.bytes, std::fs::metadata(&archive).unwrap().len());
        // Cancel an actual pg_restore while post-data refresh sleeps. Earlier
        // table/COPY work must roll back with its single transaction.
        sqlx::query("CREATE DATABASE ygg_cancel_restore").execute(&pool).await.unwrap();
        for lose_lease in [false, true] {
        let cancel_options = source_options.clone().database("ygg_cancel_restore").options([("ygg.restore_delay", "5")]);
        let cancel_bin = bin.clone();
        let cancel_receipt = receipt.clone();
        let mut cancel_archive = std::fs::File::open(&archive).unwrap();
        let restoring = tokio::spawn(async move { ygg::db::restore::database(&cancel_bin, &cancel_options, &mut cancel_archive, &cancel_receipt).await });
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let sleeping: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname='ygg_cancel_restore' AND application_name='ygg-restore' AND wait_event='PgSleep')").fetch_one(&pool).await.unwrap();
                if sleeping { break; }
                assert!(!restoring.is_finished(), "restore ended before cancellation checkpoint");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        if lose_lease {
            sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='ygg_cancel_restore' AND application_name <> 'ygg-restore'").execute(&pool).await.unwrap();
            let error = tokio::time::timeout(Duration::from_secs(5), restoring).await.unwrap().unwrap().unwrap_err();
            assert!(error.to_string().contains("lease"), "{error:#}");
        } else {
            restoring.abort();
            assert!(restoring.await.unwrap_err().is_cancelled());
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname='ygg_cancel_restore')").fetch_one(&pool).await.unwrap();
                if !active { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let cancelled = PgPoolOptions::new().max_connections(1).connect_with(source_options.clone().database("ygg_cancel_restore")).await.unwrap();
        let objects: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public'").fetch_one(&cancelled).await.unwrap();
        assert_eq!(objects, 0, "interrupted restore left partial objects");
        cancelled.close().await;
        }
        sqlx::query("CREATE DATABASE \"ygg_backup=restore λ\"").execute(&pool).await.unwrap();
        let restored_options = source_options.clone().database("ygg_backup=restore λ");
        ygg::db::restore::database(&bin, &restored_options, &mut std::fs::File::open(&archive).unwrap(), &receipt).await.unwrap();
        assert!(ygg::db::restore::database(&bin, &restored_options, &mut std::fs::File::open(&archive).unwrap(), &receipt).await.unwrap_err().to_string().contains("must be empty"));
        let restored_pool = PgPoolOptions::new().max_connections(1).connect_with(restored_options.clone()).await.unwrap();
        let id: uuid::Uuid = sqlx::query_scalar("SELECT database_id FROM knowledge_storage").fetch_one(&restored_pool).await.unwrap();
        assert_eq!(id, receipt.database_id);
        let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as("SELECT id, body FROM backup_probe").fetch_all(&restored_pool).await.unwrap();
        assert_eq!(rows, vec![(preserved_id, "before snapshot".into())]);
        let claim: (uuid::Uuid, uuid::Uuid, String, String) = sqlx::query_as("SELECT t.assignee, t.current_attempt_id, r.state::text, r.idempotency_key FROM tasks t JOIN task_runs r ON r.run_id=t.current_attempt_id WHERE t.task_id=$1").bind(claim_task).fetch_one(&restored_pool).await.unwrap();
        assert_eq!(claim, (claim_agent, claim_run, "running".into(), "restore-acknowledged-claim".into()));
        assert!(sqlx::query("INSERT INTO task_runs(task_id, attempt, idempotency_key) VALUES ($1, 2, 'restore-acknowledged-claim')").bind(claim_task).execute(&restored_pool).await.is_err());

        assert!(sqlx::query("INSERT INTO backup_probe VALUES ($1, '')").bind(uuid::Uuid::new_v4()).execute(&restored_pool).await.is_err());
        for (table, expected) in &receipt.table_rows {
            let actual: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(&restored_pool).await.unwrap();
            assert_eq!(actual, *expected, "{table}");
        }
        // Counts alone cannot detect an edited UUID/body or a removed constraint.
        sqlx::query("UPDATE backup_probe SET body='changed'").execute(&restored_pool).await.unwrap();
        assert!(ygg::db::restore::validate(&restored_options, &receipt).await.unwrap_err().to_string().contains("content or schema"));
        sqlx::query("UPDATE backup_probe SET body='before snapshot'").execute(&restored_pool).await.unwrap();
        sqlx::query("ALTER TABLE backup_probe DROP CONSTRAINT backup_probe_body_check").execute(&restored_pool).await.unwrap();
        assert!(ygg::db::restore::validate(&restored_options, &receipt).await.unwrap_err().to_string().contains("content or schema"));
        sqlx::query("ALTER TABLE backup_probe ADD CONSTRAINT backup_probe_body_check CHECK(body <> '')").execute(&restored_pool).await.unwrap();
        ygg::db::restore::validate(&restored_options, &receipt).await.unwrap();
        restored_pool.close().await;
        let mut second_output = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(temp.path().join("roundtrip.dump")).unwrap();
        let roundtrip = ygg::db::backup::dump(&bin, &restored_options, &mut second_output).await.unwrap();
        assert_eq!(roundtrip.database_id, receipt.database_id);
        assert_eq!(roundtrip.table_rows, receipt.table_rows);
        // Exercise the actual external backup CLI, including separate policy,
        // owner selection, exclusive publication and offline verification.
        let bundle_path = temp.path().join("backup-knowledge");
        let policy_path = temp.path().join("backup-policy");
        let store = ygg::knowledge::store::KnowledgeStore::open(&bundle_path, true).unwrap();
        let document = ygg::knowledge::document::Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap();
        let saved = store.put(&document, ygg::knowledge::store::ExpectedRevision::Absent).unwrap();
        let registry = ygg::knowledge::identity::IdentityRegistry::open(&policy_path, true).unwrap();
        let corpus = registry.initialize(true).unwrap().corpus_id;
        let destination = temp.path().join("deployment-backup");
        let backup_command = |destination: &std::path::Path| {
            let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
            cmd.env("DATABASE_URL", format!("postgres://ygg_runtime@localhost/postgres?host={}", root.join("runtime").display()))
                .env("YGG_DATABASE_OWNER_URL", format!("postgres://ygg_bootstrap@localhost/postgres?host={}", root.join("runtime").display()))
                .env("YGG_DB_MODE", "external").env("YGG_KNOWLEDGE_DIR", &bundle_path)
                .env("YGG_CONFIG_DIR", temp.path().join("backup-config"))
                .env("YGG_DATA_DIR", temp.path().join("backup-data"))
                .args(["db", "backup"]).arg(destination).arg("--pg-bin").arg(&bin).arg("--json");
            cmd
        };
        assert!(!backup_command(&destination).output().await.unwrap().status.success());
        assert!(!destination.exists());
        std::fs::create_dir(temp.path().join("backup-config")).unwrap();
        std::fs::write(temp.path().join("backup-config/.env"),"YGG_USER=configuration-only-fixture-secret\n").unwrap();
        let backed_up = backup_command(&destination).env("YGG_KNOWLEDGE_POLICY_DIR", &policy_path).output().await.unwrap();
        assert!(backed_up.status.success(), "{}", String::from_utf8_lossy(&backed_up.stderr));
        let combined: ygg::db::deployment_backup::Manifest = serde_json::from_slice(&backed_up.stdout).unwrap();
        assert_eq!(combined.version,2);
        assert!(combined.configuration.is_some());
        assert!(!String::from_utf8_lossy(&backed_up.stdout).contains("configuration-only-fixture-secret"));
        assert_eq!(combined.knowledge.as_ref().unwrap().corpus_id, corpus);
        assert_eq!(ygg::db::deployment_backup::verify(&destination).unwrap(), combined);
        assert!(!backup_command(&destination).arg("--policy-dir").arg(&policy_path).output().await.unwrap().status.success());
        assert_eq!(ygg::db::deployment_backup::verify(&destination).unwrap(), combined);
        assert!(!temp.path().join("backup-data").exists());
        // Restore combined backup through the real CLI into an empty external
        // target. The configured runtime cannot perform DDL; owner is explicit.
        sqlx::query("CREATE DATABASE ygg_combined_restore OWNER ygg_owner").execute(&pool).await.unwrap();
        let restored_files = temp.path().join("restored-files");
        let restore_command = || {
            let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
            cmd.env("DATABASE_URL", format!("postgres://ygg_runtime@localhost/ygg_combined_restore?host={}", root.join("runtime").display()))
                .env("YGG_DATABASE_OWNER_URL", format!("postgres://ygg_owner@localhost/ygg_combined_restore?host={}", root.join("runtime").display()))
                .env("YGG_DB_MODE", "external").env("YGG_CONFIG_DIR", temp.path().join("restore-config"))
                .env("YGG_DATA_DIR", temp.path().join("external-restore-data"))
                .args(["db", "restore"]).arg(&destination).arg("--destination").arg(&restored_files).arg("--pg-bin").arg(&bin).arg("--json");
            cmd
        };
        let restored = restore_command().output().await.unwrap();
        assert!(restored.status.success(), "{}", String::from_utf8_lossy(&restored.stderr));
        let restored_receipt: serde_json::Value = serde_json::from_slice(&restored.stdout).unwrap();
        assert_eq!(restored_receipt["configuration_switched"], false);
        assert_eq!(std::fs::read(restored_files.join("source-configuration.json")).unwrap(),std::fs::read(destination.join("configuration.json")).unwrap());
        assert!(restored_receipt["source_configuration_sha256"].is_string());
        assert_eq!(restored_receipt["database_id"], combined.database.database_id.to_string());
        assert_eq!(std::fs::read(restored_files.join("knowledge").join(saved.key.relative_path())).unwrap(), std::fs::read(destination.join("knowledge/corpus").join(saved.key.relative_path())).unwrap());
        assert_eq!(ygg::knowledge::identity::IdentityRegistry::open(&restored_files.join("policy"), false).unwrap().read().unwrap().0.corpus_id, corpus);
        assert!(!restore_command().output().await.unwrap().status.success());
        assert!(!temp.path().join("external-restore-data").exists());
        assert!(!temp.path().join("restore-config").exists());

        // External -> new managed recovery uses pinned tools, rebinds ownership,
        // grants only runtime CRUD, and leaves the target stopped for switching.
        let managed_data = temp.path().join("restored-managed");
        let managed_files = temp.path().join("restored-managed-files");
        let mut managed_restore = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
        let restored = managed_restore.env_remove("DATABASE_URL").env_remove("YGG_DATABASE_OWNER_URL")
            .env("YGG_DB_MODE", "managed").env("YGG_DATA_DIR", &managed_data)
            .env("YGG_CONFIG_DIR", temp.path().join("restore-config"))
            .args(["db", "restore"]).arg(&destination).arg("--destination").arg(&managed_files)
            .arg("--postgres-archive").arg(std::env::var("YGG_TEST_PG_ARCHIVE").unwrap()).arg("--json")
            .output().await.unwrap();
        assert!(restored.status.success(), "{}", String::from_utf8_lossy(&restored.stderr));
        let recovered = ManagedCluster::open(&managed_data.join("postgres")).unwrap();
        assert!(matches!(recovered.status().await.unwrap(), ygg::db::runtime::Status::Stopped));
        let mut recovered_owner = recovered.try_owner().unwrap().unwrap();
        recovered_owner.start_or_adopt(Duration::from_secs(30)).await.unwrap();
        let runtime_options = ygg::db::provision::runtime_options(&recovered);
        let recovered_pool = PgPoolOptions::new().max_connections(1).connect_with(runtime_options.clone()).await.unwrap();
        let id: uuid::Uuid = sqlx::query_scalar("SELECT database_id FROM knowledge_storage").fetch_one(&recovered_pool).await.unwrap();
        assert_eq!(id, combined.database.database_id);
        assert!(sqlx::query("CREATE TABLE must_fail(id int)").execute(&recovered_pool).await.is_err());
        assert!(sqlx::query("UPDATE knowledge_storage SET generation=generation+1").execute(&recovered_pool).await.is_err());
        recovered_pool.close().await;
        recovered_owner.stop(Duration::from_secs(30)).await.unwrap();
        drop(recovered_owner);
        // Select each restored deployment through the actual command. Configuration
        // publication cannot be shadowed by legacy environment defaults.
        let switch_config = temp.path().join("switch-config");
        std::fs::DirBuilder::new().mode(0o700).create(&switch_config).unwrap();
        let original_config = b"data_dir='/retained-original'\n";
        std::fs::write(switch_config.join("config.toml"), original_config).unwrap();
        let proposed = temp.path().join("proposed.toml");
        let write_proposed = |managed: bool| {
            let files = if managed { &managed_files } else { &restored_files };
            let data = if managed { managed_data.clone() } else { temp.path().join("selected-external-data") };
            let mut config = toml::Table::new();
            config.insert("data_dir".into(), toml::Value::String(data.to_str().unwrap().into()));
            config.insert("knowledge_dir".into(), toml::Value::String(files.join("knowledge").to_str().unwrap().into()));
            config.insert("knowledge_policy_dir".into(), toml::Value::String(files.join("policy").to_str().unwrap().into()));
            let mut db = toml::Table::new();
            db.insert("mode".into(), toml::Value::String(if managed { "managed" } else { "external" }.into()));
            if !managed {
                db.insert("url".into(), toml::Value::String(format!("postgres://ygg_runtime@localhost/ygg_combined_restore?host={}", root.join("runtime").display())));
                db.insert("owner_url".into(), toml::Value::String(format!("postgres://ygg_owner@localhost/ygg_combined_restore?host={}", root.join("runtime").display())));
            }
            config.insert("database".into(), toml::Value::Table(db));
            std::fs::write(&proposed, toml::to_string(&config).unwrap()).unwrap();
            std::fs::set_permissions(&proposed, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        let switch_command = |files: &std::path::Path| {
            let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
            for name in ["DATABASE_URL", "YGG_DATABASE_OWNER_URL", "YGG_DB_MODE", "YGG_DATA_DIR", "YGG_PROFILE", "YGG_KNOWLEDGE_DIR", "YGG_KNOWLEDGE_POLICY_DIR"] { cmd.env_remove(name); }
            cmd.env("YGG_CONFIG_DIR", &switch_config).args(["db", "switch"]).arg(&destination)
                .arg("--restore-dir").arg(files).arg("--target-config").arg(&proposed).arg("--json");
            cmd
        };
        write_proposed(false);
        std::fs::write(switch_config.join(".env"), "DATABASE_URL=postgres://private-secret@localhost/wrong\n").unwrap();
        let refused = switch_command(&restored_files).output().await.unwrap();
        assert!(!refused.status.success());
        assert!(!String::from_utf8_lossy(&refused.stderr).contains("private-secret"));
        assert_eq!(std::fs::read(switch_config.join("config.toml")).unwrap(), original_config);
        std::fs::remove_file(switch_config.join(".env")).unwrap();
        // Restored external runtime has no ACLs until the operator grants them.
        let refused = switch_command(&restored_files).output().await.unwrap();
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("runtime"));
        assert_eq!(std::fs::read(switch_config.join("config.toml")).unwrap(), original_config);
        let grants = PgPoolOptions::new().max_connections(1).connect_with(source_options.clone().database("ygg_combined_restore")).await.unwrap();
        sqlx::query("GRANT USAGE ON SCHEMA public TO ygg_runtime").execute(&grants).await.unwrap();
        sqlx::query("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO ygg_runtime").execute(&grants).await.unwrap();
        sqlx::query("REVOKE ALL ON public.knowledge_storage, public._sqlx_migrations FROM ygg_runtime").execute(&grants).await.unwrap();
        sqlx::query("GRANT SELECT ON public.knowledge_storage, public._sqlx_migrations TO ygg_runtime").execute(&grants).await.unwrap();
        grants.close().await;
        let selected = switch_command(&restored_files).output().await.unwrap();
        assert!(selected.status.success(), "{}", String::from_utf8_lossy(&selected.stderr));
        assert_eq!(std::fs::read(switch_config.join("config.toml")).unwrap(), std::fs::read(&proposed).unwrap());
        write_proposed(true);
        std::fs::write(managed_files.join("knowledge/unexpected"), b"independent edit").unwrap();
        assert!(!switch_command(&managed_files).output().await.unwrap().status.success());
        std::fs::remove_file(managed_files.join("knowledge/unexpected")).unwrap();
        let selected = switch_command(&managed_files).output().await.unwrap();
        assert!(selected.status.success(), "{}", String::from_utf8_lossy(&selected.stderr));
        let outcome: serde_json::Value = serde_json::from_slice(&selected.stdout).unwrap();
        assert_eq!(outcome["already_applied"], false);
        assert_eq!(std::fs::read(switch_config.join("config.toml")).unwrap(), std::fs::read(&proposed).unwrap());
        assert!(matches!(recovered.status().await.unwrap(), ygg::db::runtime::Status::Ready{..}));
        let application = ygg::config::database::DeploymentConfig::load(std::collections::BTreeMap::from([("YGG_CONFIG_DIR".into(), switch_config.to_str().unwrap().into())])).unwrap();
        assert_eq!(application.knowledge_policy_dir, managed_files.join("policy"));
        assert!(matches!(application.database, ygg::config::database::DatabaseTarget::ManagedLocal{data_dir} if data_dir==managed_data));
        // A confirmed switch can resume after new writes without replaying the
        // restore or trying to certify those later writes as part of the backup.
        let active = PgPoolOptions::new().max_connections(1).connect_with(runtime_options).await.unwrap();
        sqlx::query("UPDATE tasks SET title='after activation' WHERE task_id=$1").bind(claim_task).execute(&active).await.unwrap();
        active.close().await;
        ygg::db::restore::validate(&source_options, &combined.database).await.unwrap();
        let resumed = switch_command(&managed_files).arg("--resume").arg(outcome["operation"].as_str().unwrap()).output().await.unwrap();
        assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&resumed.stdout).unwrap()["already_applied"], true);
        ygg::db::supervisor::stop(&recovered, Duration::from_secs(30)).await.unwrap();
        // Force a storage-generation change after the database snapshot but
        // before the paired filesystem snapshot. No combined backup may publish.
        let policy_lease = std::fs::OpenOptions::new().read(true).write(true).open(policy_path.join(".writer.lock")).unwrap();
        fs2::FileExt::lock_exclusive(&policy_lease).unwrap();
        let racing_destination = temp.path().join("racing-backup");
        let mut racing = backup_command(&racing_destination).arg("--policy-dir").arg(&policy_path)
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let copying = std::fs::read_dir(temp.path()).unwrap().flatten().any(|entry| entry.file_name().to_string_lossy().starts_with(".deployment-backup-") && entry.path().join("database.dump").metadata().is_ok_and(|m| m.len() > 5));
                if copying { break; }
                assert!(racing.try_wait().unwrap().is_none(), "backup exited before dump stage");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap();
        sqlx::query("UPDATE knowledge_storage SET generation = generation + 1").execute(&pool).await.unwrap();
        fs2::FileExt::unlock(&policy_lease).unwrap();
        let failed = racing.wait_with_output().await.unwrap();
        assert!(!failed.status.success());
        assert!(String::from_utf8_lossy(&failed.stderr).contains("generation changed"), "{}", String::from_utf8_lossy(&failed.stderr));
        assert!(!racing_destination.exists());

        let verify_command = || {
            let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
            cmd.env("YGG_DB_MODE", "invalid-for-offline-check")
                .env("DATABASE_URL", "not-a-database-secret")
                .args(["db", "verify-backup"]).arg(&destination).arg("--json");
            cmd
        };
        assert!(verify_command().output().await.unwrap().status.success());
        std::fs::write(destination.join("knowledge/corpus").join(saved.key.relative_path()), "tampered").unwrap();
        assert!(!verify_command().output().await.unwrap().status.success());

        let runtime = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(ygg::db::provision::runtime_options(&cluster))
            .await
            .unwrap();
        let user: String = sqlx::query_scalar("SELECT current_user")
            .fetch_one(&runtime)
            .await
            .unwrap();
        assert_eq!(user, "ygg_runtime");
        let mut authority =
            ygg::db::singleton::SingletonGuard::try_acquire(&runtime, 0x59504754455354)
                .await
                .unwrap()
                .unwrap();
        authority.verify().await.unwrap();
        drop(authority);
        assert!(
            ygg::db::pending_migrations(&runtime)
                .await
                .unwrap()
                .is_empty()
        );
        let mut guarded = ygg::knowledge::guard::legacy_transaction(&runtime, true, None)
            .await
            .unwrap();
        sqlx::query("INSERT INTO memories (text) VALUES ('limited-role note')")
            .execute(&mut *guarded)
            .await
            .unwrap();
        guarded.commit().await.unwrap();
        for forbidden in [
            "CREATE TABLE public.forbidden (id integer)",
            "CREATE TEMP TABLE forbidden (id integer)",
            "CREATE DATABASE forbidden",
            "CREATE ROLE forbidden",
            "SET ROLE ygg_owner",
            "SET ROLE ygg_bootstrap",
            "UPDATE knowledge_storage SET generation = generation + 1",
            "DELETE FROM _sqlx_migrations",
            "ALTER TABLE memories DISABLE TRIGGER ALL",
            "TRUNCATE memories",
        ] {
            let error = sqlx::query(forbidden).execute(&runtime).await.unwrap_err();
            assert_eq!(
                error.as_database_error().and_then(|e| e.code()).as_deref(),
                Some("42501"),
                "{forbidden}: {error}"
            );
        }
        // Drift is reported, not silently repaired or adopted.
        sqlx::query("ALTER ROLE ygg_runtime SUPERUSER")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            ygg::db::provision::migrate(&cluster)
                .await
                .unwrap_err()
                .to_string()
                .contains("unexpected privilege")
        );
        sqlx::query("ALTER ROLE ygg_runtime NOSUPERUSER")
            .execute(&pool)
            .await
            .unwrap();
        runtime.close().await;
        pool.close().await;
    })
    .catch_unwind()
    .await;
    let stopped = owner.stop(Duration::from_secs(10)).await;
    if stopped.is_err() {
        let _ = tokio::process::Command::new(bin.join("pg_ctl"))
            .arg("-D")
            .arg(root.join("data"))
            .args(["stop", "-m", "fast", "-w", "-t", "5"])
            .output()
            .await;
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    stopped.unwrap();
    let edited = bin
        .parent()
        .unwrap()
        .join("share/extension/uuid-ossp.control");
    std::fs::write(&edited, "independent edit").unwrap();
    assert!(package::install_offline(&base, &archive).is_err());
    assert_eq!(std::fs::read_to_string(edited).unwrap(), "independent edit");
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_ARCHIVE pointing to the exact native pinned archive"]
async fn real_init_installs_offline_converges_and_reuses_cluster_without_archive() {
    use ygg::db::supervisor;
    let archive = PathBuf::from(std::env::var("YGG_TEST_PG_ARCHIVE").unwrap());
    let temp = tempfile::Builder::new()
        .prefix("yini-")
        .tempdir_in("/tmp")
        .unwrap();
    let data = temp.path().canonicalize().unwrap().join("data");
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let command = || {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
        cmd.env("HOME", &home)
            .env("YGG_DATA_DIR", &data)
            .env("YGG_CONFIG_DIR", home.join("config"))
            .env("YGG_DB_MODE", "managed")
            .env_remove("DATABASE_URL")
            .env_remove("YGG_PROFILE")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        cmd
    };
    let result = std::panic::AssertUnwindSafe(async {
        let mut first = command();
        first
            .args([
                "init",
                "--yes",
                "--skip",
                "tmux,jq,rtk,hooks,project",
                "--postgres-archive",
            ])
            .arg(&archive);
        let mut second = command();
        second
            .args([
                "init",
                "--yes",
                "--skip",
                "tmux,jq,rtk,hooks,project",
                "--postgres-archive",
            ])
            .arg(&archive);
        let (first, second) = tokio::join!(first.output(), second.output());
        for output in [first.unwrap(), second.unwrap()] {
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let cluster = ManagedCluster::open(&data.join("postgres")).unwrap();
        let identity = cluster.id();
        let runtime = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(ygg::db::provision::runtime_options(&cluster))
            .await
            .unwrap();
        let user: String = sqlx::query_scalar("SELECT current_user")
            .fetch_one(&runtime)
            .await
            .unwrap();
        assert_eq!(user, "ygg_runtime");
        assert!(
            ygg::db::pending_migrations(&runtime)
                .await
                .unwrap()
                .is_empty()
        );
        runtime.close().await;
        let status = cluster.status().await.unwrap();
        // Existing clusters require neither the input archive nor a download.
        let output = command()
            .args(["init", "--yes", "--skip", "tmux,jq,rtk,hooks,project"])
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            ManagedCluster::open(&data.join("postgres")).unwrap().id(),
            identity
        );
        assert_eq!(cluster.status().await.unwrap(), status);
        assert!(
            !home.join("config/.env").exists(),
            "managed init must not manufacture a localhost URL"
        );
        let backup_path = temp.path().join("managed-backup");
        let backup = command()
            .args(["db", "backup"])
            .arg(&backup_path)
            .arg("--json")
            .output()
            .await
            .unwrap();
        assert!(
            backup.status.success(),
            "{}",
            String::from_utf8_lossy(&backup.stderr)
        );
        let manifest = ygg::db::deployment_backup::verify(&backup_path).unwrap();
        assert!(manifest.knowledge.is_none());
        assert_eq!(cluster.status().await.unwrap(), status);
        supervisor::stop(&cluster, Duration::from_secs(15))
            .await
            .unwrap();
    })
    .catch_unwind()
    .await;
    if let Ok(cluster) = ManagedCluster::open(&data.join("postgres")) {
        let _ = supervisor::stop(&cluster, Duration::from_secs(15)).await;
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
