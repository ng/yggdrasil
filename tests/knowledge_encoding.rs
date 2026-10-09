#![cfg(any(target_os = "macos", target_os = "linux"))]
//! Real non-UTF8 database admission, independent of client-side UTF8 decoding.
use futures::FutureExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::BTreeMap, process::Command};
use uuid::Uuid;
use ygg::knowledge::{
    document::digest, export, forward, inventory, legacy::Mappings, reverse, store::Snapshot,
};

fn rejected<T>(result: anyhow::Result<T>) {
    let error = result.err().expect("non-UTF8 migration must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("requires UTF8 server encoding"),
        "{message}"
    );
    assert!(message.contains("SQL_ASCII"), "{message}");
}
async fn state(pool: &PgPool) -> serde_json::Value {
    sqlx::query_scalar("SELECT jsonb_build_object('marker',(SELECT to_jsonb(s) FROM public.knowledge_storage s WHERE singleton),'notes',(SELECT jsonb_agg(to_jsonb(m) ORDER BY memory_id) FROM public.memories m),'usage',(SELECT count(*) FROM public.knowledge_usage),'forward',(SELECT count(*) FROM public.knowledge_forward_receipts),'reverse',(SELECT count(*) FROM public.knowledge_reverse_receipts))")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn non_utf8_migration_refuses_without_changing_legacy_data_or_markers() {
    let base = std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let database = format!("ygg_encoding_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database} TEMPLATE template0 ENCODING 'SQL_ASCII' LOCALE_PROVIDER libc LC_COLLATE 'C' LC_CTYPE 'C'"))
        .execute(&admin).await.unwrap();
    let mut url = url::Url::parse(&base).unwrap();
    url.set_path(&format!("/{database}"));
    let url = url.to_string();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        ygg::db::run_migrations(&pool).await.unwrap();
        let server: String = sqlx::query_scalar("SHOW server_encoding")
            .fetch_one(&pool)
            .await
            .unwrap();
        let client: String = sqlx::query_scalar("SHOW client_encoding")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(server, "SQL_ASCII");
        assert_eq!(
            client, "UTF8",
            "client encoding cannot establish server semantics"
        );
        let sql_matches: bool = sqlx::query_scalar("SELECT $1::text LIKE '_'")
            .bind("λ")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!sql_matches);
        assert!(
            ygg::knowledge::matching::file_pattern("_")
                .unwrap()
                .is_match("λ")
        );
        let memories = ygg::models::memory::MemoryRepo::new(&pool);
        memories
            .create(None, "legacy access remains available", None)
            .await
            .unwrap();
        let before = state(&pool).await;
        let database_id: Uuid =
            sqlx::query_scalar("SELECT database_id FROM public.knowledge_storage WHERE singleton")
                .fetch_one(&pool)
                .await
                .unwrap();
        let mappings = Mappings {
            database_id,
            corpus_id: Uuid::new_v4(),
            repos: BTreeMap::new(),
            users: BTreeMap::from([(String::new(), "owner".into())]),
        };
        rejected(inventory::assess(&pool, Some(&mappings)).await);
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("stage");
        rejected(export::stage(&pool, &mappings, &stage).await);
        assert!(!stage.exists());
        let manifest = export::Manifest {
            version: 1,
            database_id,
            generation: 1,
            corpus_id: mappings.corpus_id,
            mappings: serde_json::to_value(&mappings).unwrap(),
            entries: Vec::new(),
        };
        let mut tx = pool.begin().await.unwrap();
        rejected(forward::activate_on(&mut tx, Uuid::new_v4(), &manifest).await);
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        rejected(reverse::capture_on(&mut tx, &manifest, &Snapshot::default(), 2).await);
        tx.commit().await.unwrap();
        let candidate = reverse::Candidate {
            version: 1,
            database_id,
            corpus_id: mappings.corpus_id,
            export_generation: 1,
            export_digest: digest(&serde_json::to_vec(&manifest).unwrap()),
            documents: Vec::new(),
            deleted: Vec::new(),
            notes: Vec::new(),
            learnings: Vec::new(),
        };
        let evidence = reverse::RecoveryEvidence {
            version: 1,
            candidate_sha256: digest(&serde_json::to_vec(&candidate).unwrap()),
            corpus_revision: "0".repeat(64),
            policy_revision: "0".repeat(64),
            shared_commit: None,
        };
        let mut tx = pool.begin().await.unwrap();
        rejected(reverse::apply_on(&mut tx, &candidate, 2).await);
        rejected(reverse::apply_once_on(&mut tx, Uuid::new_v4(), &candidate, &evidence, 2).await);
        tx.commit().await.unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ygg"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", temp.path())
            .env("YGG_CONFIG_DIR", temp.path().join("config"))
            .env("DATABASE_URL", &url)
            .env("YGG_DB_MODE", "external")
            .args(["knowledge", "migrate", "--dry-run", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires UTF8 server encoding"));
        // Owner SQL callers cannot bypass admission by omitting the Rust layer.
        // Fencing itself remains reversible, including for older interrupted work.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("UPDATE public.knowledge_storage SET backend='fenced',corpus_id=$1,generation=generation+1 WHERE singleton")
            .bind(mappings.corpus_id).execute(&mut *tx).await.unwrap();
        sqlx::query("SAVEPOINT before_activation").execute(&mut *tx).await.unwrap();
        let error = sqlx::query("UPDATE public.knowledge_storage SET backend='okf',generation=generation+1 WHERE singleton")
            .execute(&mut *tx).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("0A000"));
        sqlx::query("ROLLBACK TO SAVEPOINT before_activation").execute(&mut *tx).await.unwrap();
        sqlx::query("SAVEPOINT before_import").execute(&mut *tx).await.unwrap();
        let error = sqlx::query("SELECT public.ygg_knowledge_reverse_import($1,$2,2,'[]'::jsonb,'[]'::jsonb)")
            .bind(database_id).bind(mappings.corpus_id).execute(&mut *tx).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("0A000"));
        sqlx::query("ROLLBACK TO SAVEPOINT before_import").execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE public.knowledge_storage SET backend='sql',corpus_id=NULL,generation=generation+1 WHERE singleton")
            .execute(&mut *tx).await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(before, state(&pool).await);
        let corpus = temp.path().join("corpus");
        let policy = temp.path().join("policy");
        let journal = temp.path().join("journal");
        let plan_file = temp.path().join("plan.json");
        let plan = ygg::knowledge::migration::Plan {
            version: 1,
            transport: "private".into(),
            source_generation: 1,
            identities: ygg::knowledge::identity::Identities {
                version: 1,
                corpus_id: mappings.corpus_id,
                trusted: true,
                approval_leads: Default::default(),
                repos: Vec::new(),
            },
            mappings,
            agents: BTreeMap::new(),
            execution_host: "fixture".into(),
            all_participating_hosts_listed: true,
            hosts: vec![ygg::knowledge::migration::Host {
                name: "fixture".into(),
                protocol: ygg::knowledge::guard::CLIENT_PROTOCOL,
                knowledge_writers_stopped: true,
                external_editors_stopped: true,
                schema_changes_stopped: true,
                session_preserving_endpoint: true,
            }],
        };
        std::fs::write(&plan_file, serde_json::to_vec(&plan).unwrap()).unwrap();
        for _ in 0..2 {
            let output = Command::new(env!("CARGO_BIN_EXE_ygg"))
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", temp.path())
                .env("YGG_CONFIG_DIR", temp.path().join("config"))
                .env("YGG_DATA_DIR", temp.path().join("data"))
                .env("YGG_KNOWLEDGE_DIR", &corpus)
                .env("YGG_KNOWLEDGE_POLICY_DIR", &policy)
                .env("DATABASE_URL", &url)
                .env("YGG_DATABASE_OWNER_URL", &url)
                .env("YGG_DB_MODE", "external")
                .args(["knowledge", "migrate", "--plan"])
                .arg(&plan_file)
                .arg("--journal")
                .arg(&journal)
                .arg("--json")
                .output()
                .unwrap();
            assert!(!output.status.success());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("requires UTF8 server encoding"), "{stderr}");
            assert!(!journal.join("source-backup").exists());
            assert!(!journal.join("stage").exists());
            assert!(!policy.join("knowledge.json").exists());
        }
        assert_eq!(before, state(&pool).await);
        let rows = memories.list(None, true, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "legacy access remains available");
    })
    .catch_unwind()
    .await;
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
