#![cfg(any(target_os = "macos", target_os = "linux"))]
//! Exercise transaction boundaries on a disposable database, including recovery
//! after the caller lost a successful commit result.
use futures::FutureExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::collections::BTreeMap;
use uuid::Uuid;
use ygg::knowledge::{
    clients, export,
    forward::{self, Outcome},
    inventory,
    legacy::Mappings,
    telemetry::Telemetry,
};

struct Fixture {
    admin: PgPool,
    pool: PgPool,
    database: String,
    url: String,
}
impl Fixture {
    async fn new() -> Self {
        let base = std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&base)
            .await
            .unwrap();
        let database = format!("ygg_forward_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&base).unwrap();
        url.set_path(&format!("/{database}"));
        let url = url.to_string();
        // No nested pool checkout can succeed while activation owns its tx.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        ygg::db::run_migrations(&pool).await.unwrap();
        clients::register(&mut pool.acquire().await.unwrap())
            .await
            .unwrap();
        Self {
            admin,
            pool,
            database,
            url,
        }
    }
    async fn stage(&self, path: &std::path::Path) -> export::Manifest {
        let source = inventory::assess(&self.pool, None).await.unwrap().source;
        let mappings = Mappings {
            database_id: source.database_id,
            corpus_id: Uuid::new_v4(),
            repos: BTreeMap::new(),
            users: BTreeMap::from([("alice".into(), "owner".into())]),
        };
        sqlx::query(
            "UPDATE public.knowledge_storage SET backend='fenced',generation=2,corpus_id=$1",
        )
        .bind(mappings.corpus_id)
        .execute(&self.pool)
        .await
        .unwrap();
        export::stage(&self.pool, &mappings, path).await.unwrap()
    }
    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.database))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

#[tokio::test]
async fn activation_is_atomic_and_uncertain_commit_retries_preserve_observations() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let rules = [Uuid::new_v4(), Uuid::new_v4()];
        for rule in rules {
            sqlx::query("INSERT INTO public.learnings(learning_id,text,user_id,status,source,applied_count,last_applied_at) VALUES($1,'pending rule','alice','pending','proposed',7,'2026-01-01T00:00:00Z')")
                .bind(rule).execute(&f.pool).await.unwrap();
        }
        sqlx::query("INSERT INTO public.memories(text,user_id) VALUES('note λ','alice')").execute(&f.pool).await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let manifest = f.stage(&temp.path().join("stage")).await;
        let operation = Uuid::new_v4();
        let corpus = manifest.corpus_id;
        let telemetry = Telemetry::new(&f.pool);
        telemetry.record(corpus, rules[0], Uuid::new_v4(), chrono::Utc::now()).await.unwrap();
        // One conflicting baseline must undo other successful inserts/updates,
        // even if the caller commits after the returned Rust validation error.
        sqlx::query("INSERT INTO public.knowledge_usage(corpus_id,document_id,imported_count) VALUES($1,$2,999)")
            .bind(corpus).bind(rules[1]).execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        let error = forward::activate_on(&mut tx, operation, &manifest).await.unwrap_err();
        assert!(error.to_string().contains("conflicting imported"), "{error:#}");
        tx.commit().await.unwrap();
        assert!(!telemetry.get(corpus, rules[0]).await.unwrap().unwrap().baseline_imported);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM public.knowledge_forward_receipts").fetch_one(&f.pool).await.unwrap(), 0);
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT backend FROM public.knowledge_storage").fetch_one(&f.pool).await.unwrap(), "fenced");
        sqlx::query("DELETE FROM public.knowledge_usage WHERE document_id=$1").bind(rules[1]).execute(&f.pool).await.unwrap();
        // Outer rollback undoes the entire successful SQL half as well.
        let mut tx = f.pool.begin().await.unwrap();
        assert_eq!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap(), Outcome::Activated);
        tx.rollback().await.unwrap();
        assert!(!telemetry.get(corpus, rules[0]).await.unwrap().unwrap().baseline_imported);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM public.knowledge_forward_receipts").fetch_one(&f.pool).await.unwrap(), 0);
        // A stale manifest and duplicate document identity cannot select OKF.
        let mut changed: export::Manifest = serde_json::from_value(serde_json::to_value(&manifest).unwrap()).unwrap();
        changed.entries[0].source_digest = "0".repeat(64);
        let mut tx = f.pool.begin().await.unwrap();
        assert!(forward::activate_on(&mut tx, operation, &changed).await.is_err());
        tx.commit().await.unwrap();
        let duplicate = serde_json::from_value(serde_json::to_value(&manifest.entries[0]).unwrap()).unwrap();
        changed.entries.push(duplicate);
        let mut tx = f.pool.begin().await.unwrap();
        assert!(forward::activate_on(&mut tx, operation, &changed).await.unwrap_err().to_string().contains("duplicate"));
        tx.rollback().await.unwrap();
        // Unknown old connection is rejected under the migration lease.
        let old = PgPoolOptions::new().max_connections(1).connect(&f.url).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap_err().to_string().contains("live clients"));
        tx.rollback().await.unwrap();
        old.close().await;
        let mut tx = f.pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").execute(&mut *tx).await.unwrap();
        assert!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap_err().to_string().contains("READ COMMITTED"));
        tx.rollback().await.unwrap();
        // A retry must wait for the first outcome rather than repeat the seed.
        let second = PgPoolOptions::new().max_connections(1)
            .after_connect(|c,_| Box::pin(clients::register(c))).connect(&f.url).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert_eq!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap(), Outcome::Activated);
        let copy: export::Manifest = serde_json::from_value(serde_json::to_value(&manifest).unwrap()).unwrap();
        let mut retry = tokio::spawn(async move {
            let mut tx = second.begin().await.unwrap();
            let result = forward::activate_on(&mut tx, operation, &copy).await.unwrap();
            tx.commit().await.unwrap();
            second.close().await;
            result
        });
        assert!(tokio::time::timeout(std::time::Duration::from_millis(150), &mut retry).await.is_err());
        tx.commit().await.unwrap();
        assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(10), retry).await.unwrap().unwrap(), Outcome::PreviouslyActivated);
        telemetry.record(corpus, rules[0], Uuid::new_v4(), chrono::Utc::now()).await.unwrap();
        let before = telemetry.get(corpus, rules[0]).await.unwrap().unwrap();
        assert_eq!(before.applied_count, 9);
        let mut tx = f.pool.begin().await.unwrap();
        assert_eq!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap(), Outcome::PreviouslyActivated);
        tx.commit().await.unwrap();
        assert_eq!(telemetry.get(corpus, rules[0]).await.unwrap().unwrap(), before);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM public.knowledge_forward_receipts").fetch_one(&f.pool).await.unwrap(), 1);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM public.learnings WHERE status='pending'").fetch_one(&f.pool).await.unwrap(), 2);
        assert!(sqlx::query("INSERT INTO public.memories(text) VALUES('old writer')").execute(&f.pool).await.is_err());
        // Active marker alone is insufficient: exact receipt and baseline matter.
        let mut tx = f.pool.begin().await.unwrap();
        assert!(forward::activate_on(&mut tx, Uuid::new_v4(), &manifest).await.is_err());
        tx.rollback().await.unwrap();
        sqlx::query("DELETE FROM public.knowledge_usage WHERE document_id=$1").bind(rules[1]).execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap_err().to_string().contains("telemetry changed"));
        tx.commit().await.unwrap();
        assert!(telemetry.get(corpus, rules[1]).await.unwrap().is_none());
        for mutation in ["DELETE FROM public.knowledge_forward_receipts", "UPDATE public.knowledge_forward_receipts SET manifest_sha256=repeat('0',64)", "TRUNCATE public.knowledge_forward_receipts"] {
            assert!(sqlx::query(mutation).execute(&f.pool).await.is_err());
        }
        sqlx::query("UPDATE public.knowledge_storage SET backend='fenced',generation=4").execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap_err().to_string().contains("no longer"));
        tx.rollback().await.unwrap();
    }).catch_unwind().await;
    f.cleanup().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn empty_corpus_still_checks_schema_and_runtime_cannot_activate() {
    let f = Fixture::new().await;
    let role = format!("forward_runtime_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'fixture-only-password'"
    ))
    .execute(&f.admin)
    .await
    .unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        let temp = tempfile::tempdir().unwrap();
        let manifest = f.stage(&temp.path().join("stage")).await;
        let operation = Uuid::new_v4();
        sqlx::query("ALTER TABLE public.memories ADD COLUMN future TEXT").execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap_err().to_string().contains("schema"));
        tx.commit().await.unwrap();
        sqlx::query("ALTER TABLE public.memories DROP COLUMN future").execute(&f.pool).await.unwrap();
        let mut url = url::Url::parse(&f.url).unwrap();
        url.set_username(&role).unwrap(); url.set_password(Some("fixture-only-password")).unwrap();
        let runtime = PgPoolOptions::new().max_connections(1).connect(url.as_str()).await.unwrap();
        let mut tx = runtime.begin().await.unwrap();
        assert!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap_err().to_string().contains("migration owner"));
        tx.commit().await.unwrap();
        // An accidental table grant cannot manufacture owner receipts.
        sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {role}")).execute(&f.pool).await.unwrap();
        sqlx::query(&format!("GRANT ALL ON public.knowledge_forward_receipts TO {role}")).execute(&f.pool).await.unwrap();
        let error = sqlx::query("INSERT INTO public.knowledge_forward_receipts(operation_id,database_id,corpus_id,fenced_generation,active_generation,manifest_sha256) VALUES($1,$2,$3,2,3,repeat('0',64))")
            .bind(operation).bind(manifest.database_id).bind(manifest.corpus_id).execute(&runtime).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("42501"));
        runtime.close().await;
        let mut tx = f.pool.begin().await.unwrap();
        assert_eq!(forward::activate_on(&mut tx, operation, &manifest).await.unwrap(), Outcome::Activated);
        tx.commit().await.unwrap();
    }).catch_unwind().await;
    sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", f.database))
        .execute(&f.admin)
        .await
        .unwrap();
    sqlx::query(&format!("DROP ROLE {role}"))
        .execute(&f.admin)
        .await
        .unwrap();
    f.pool.close().await;
    f.admin.close().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn queued_legacy_writer_does_not_invert_activation_lock_order() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let temp = tempfile::tempdir().unwrap();
        let manifest = f.stage(&temp.path().join("stage")).await;
        let writer = PgPoolOptions::new().max_connections(1)
            .after_connect(|c,_| Box::pin(clients::register(c))).connect(&f.url).await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&writer).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)").execute(&mut *tx).await.unwrap();
        let write = tokio::spawn(async move {
            let result = sqlx::query("INSERT INTO public.memories(text) VALUES('queued writer')").execute(&writer).await;
            writer.close().await;
            result
        });
        // Wait until the SQL executor holds RowExclusive and is waiting for the
        // knowledge advisory lease. A SHARE source-table lock would deadlock.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_catalog.pg_locks WHERE pid=$1 AND locktype='advisory' AND NOT granted)")
                    .bind(pid).fetch_one(&f.admin).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(5), forward::activate_on(&mut tx, Uuid::new_v4(), &manifest)).await.unwrap().unwrap(), Outcome::Activated);
        tx.commit().await.unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), write).await.unwrap().unwrap().unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("55000"));
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM public.memories").fetch_one(&f.pool).await.unwrap(), 0);
    }).catch_unwind().await;
    f.cleanup().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
