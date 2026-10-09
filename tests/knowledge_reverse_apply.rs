#![cfg(any(target_os = "macos", target_os = "linux"))]
use chrono::{SubsecRound, Utc};
use futures::FutureExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::collections::BTreeMap;
use uuid::Uuid;
use ygg::knowledge::{
    document::ActivationKind,
    export, inventory,
    legacy::{self, Mappings},
    reverse,
    store::{ExpectedRevision, Kind, KnowledgeStore},
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
        let database = format!("ygg_reverse_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&base).unwrap();
        url.set_path(&format!("/{database}"));
        let url = url.to_string();
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        ygg::db::run_migrations(&pool).await.unwrap();
        Self {
            admin,
            pool,
            database,
            url,
        }
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
async fn fenced_owner_apply_preserves_current_rows_deletions_json_null_and_approvals() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let note = Uuid::new_v4(); let deleted = Uuid::new_v4(); let rule = Uuid::new_v4(); let actor = Uuid::new_v4();
        sqlx::query("INSERT INTO agents(agent_id,agent_name) VALUES($1,'reverse reviewer')").bind(actor).execute(&f.pool).await.unwrap();
        for id in [note, deleted] { sqlx::query("INSERT INTO memories(memory_id,text,user_id) VALUES($1,'old note','alice')").bind(id).execute(&f.pool).await.unwrap(); }
        sqlx::query("INSERT INTO learnings(learning_id,text,user_id,status,source,scope_tags,applied_count) VALUES($1,'old rule','alice','pending','proposed','null'::jsonb,5)").bind(rule).execute(&f.pool).await.unwrap();
        let source = inventory::assess(&f.pool, None).await.unwrap().source;
        let mappings = Mappings { database_id: source.database_id, corpus_id: Uuid::new_v4(), repos: BTreeMap::new(), users: BTreeMap::from([("alice".into(), "owner".into())]) };
        sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=2,corpus_id=$1").bind(mappings.corpus_id).execute(&f.pool).await.unwrap();
        let temp = tempfile::tempdir().unwrap(); let stage = temp.path().join("bundle");
        let manifest = export::stage(&f.pool, &mappings, &stage).await.unwrap();
        sqlx::query("UPDATE knowledge_storage SET backend='okf',generation=3").execute(&f.pool).await.unwrap();
        let store = KnowledgeStore::open(&stage, false).unwrap();
        let snapshot = store.snapshot();
        let mut usage = BTreeMap::new();
        let at = Utc::now().trunc_subsecs(6);
        for old in snapshot.documents {
            if old.key.id == deleted { store.delete(old.key, &old.revision).unwrap(); continue; }
            let mut doc = old.document;
            doc.body = if old.key.id == note { "current note λ\n" } else { "reviewed current rule\n" }.into();
            if old.key.kind == Kind::Learning {
                doc.activate(mappings.corpus_id, ActivationKind::Reviewed, Some(actor), Some(at)).unwrap();
                let mut totals = manifest.entries.iter().find(|e| e.key.id == rule).unwrap().usage.clone().unwrap();
                totals.applied_count = 12; totals.last_applied_at = Some(at); usage.insert(rule, totals);
            }
            store.put(&doc, ExpectedRevision::Digest(&old.revision)).unwrap();
        }
        let added = Uuid::new_v4();
        let new = ygg::models::memory::Memory { memory_id: added, repo_id: None, text: "new note".into(), created_by: Some(actor), created_at: at };
        store.put(&legacy::import_note(&new, "alice", &mappings).unwrap(), ExpectedRevision::Absent).unwrap();
        let candidate = reverse::build(&manifest, &store.snapshot(), &usage).unwrap();
        sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=4").execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(reverse::apply_on(&mut tx, &candidate, 3).await.is_err()); tx.rollback().await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        reverse::apply_on(&mut tx, &candidate, 4).await.unwrap();
        // Caller rollback must undo every row change and retain the write fence.
        tx.rollback().await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT text FROM memories WHERE memory_id=$1").bind(note).fetch_one(&f.pool).await.unwrap(), "old note");
        let mut tx = f.pool.begin().await.unwrap();
        reverse::apply_on(&mut tx, &candidate, 4).await.unwrap();
        assert_ne!(sqlx::query_scalar::<_,String>("SELECT current_setting('ygg.knowledge_reverse_import',true)").fetch_one(&mut *tx).await.unwrap(), "on");
        sqlx::query("SAVEPOINT ordinary_write").execute(&mut *tx).await.unwrap();
        assert!(sqlx::query("INSERT INTO memories(text) VALUES('must remain fenced')").execute(&mut *tx).await.is_err());
        sqlx::query("ROLLBACK TO ordinary_write").execute(&mut *tx).await.unwrap(); tx.commit().await.unwrap();
        let actual: Vec<ygg::models::memory::Memory> = sqlx::query_as("SELECT memory_id,repo_id,text,created_by,created_at FROM memories ORDER BY memory_id").fetch_all(&f.pool).await.unwrap();
        let expected: Vec<ygg::models::memory::Memory> = candidate.notes.iter().cloned().map(|v| serde_json::from_value(v).unwrap()).collect();
        assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
        let actual: ygg::models::learning::Learning = sqlx::query_as("SELECT learning_id,repo_id,file_glob,rule_id,text,context,created_by,created_at,applied_count,last_applied_at,scope_tags,status,source,approved_at,approved_by FROM learnings").fetch_one(&f.pool).await.unwrap();
        let expected: ygg::models::learning::Learning = serde_json::from_value(candidate.learnings[0].clone()).unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
        assert!(sqlx::query_scalar::<_,bool>("SELECT scope_tags='null'::jsonb AND scope_tags IS NOT NULL FROM learnings").fetch_one(&f.pool).await.unwrap());
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT backend FROM knowledge_storage").fetch_one(&f.pool).await.unwrap(), "fenced");
        // Repeat the same candidate while fenced; matching retained UUIDs persist.
        let mut tx = f.pool.begin().await.unwrap(); reverse::apply_on(&mut tx, &candidate, 4).await.unwrap(); tx.commit().await.unwrap();
        let mut bad = serde_json::to_value(&candidate.notes).unwrap(); bad[0]["future"] = true.into();
        let call = "SELECT ygg_knowledge_reverse_import($1,$2,4,$3,$4)";
        assert!(sqlx::query(call).bind(mappings.database_id).bind(mappings.corpus_id).bind(&bad).bind(serde_json::to_value(&candidate.learnings).unwrap()).execute(&f.pool).await.is_err());
        let mut bad = serde_json::to_value(&candidate.notes).unwrap(); bad[0]["created_at"] = "2026-01-01T00:00:00.123456789Z".into();
        assert!(sqlx::query(call).bind(mappings.database_id).bind(mappings.corpus_id).bind(&bad).bind(serde_json::to_value(&candidate.learnings).unwrap()).execute(&f.pool).await.is_err());
        // A constraint failure after deletes/updates must roll back the statement.
        let mut bad = serde_json::to_value(&candidate.learnings).unwrap(); bad[0]["approved_by"] = Uuid::new_v4().to_string().into();
        assert!(sqlx::query(call).bind(mappings.database_id).bind(mappings.corpus_id).bind(serde_json::json!([])).bind(bad).execute(&f.pool).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM memories").fetch_one(&f.pool).await.unwrap(), 2);
        sqlx::query("ALTER TABLE memories ADD COLUMN future TEXT").execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap(); assert!(reverse::apply_on(&mut tx, &candidate, 4).await.is_err()); tx.rollback().await.unwrap();
    }).catch_unwind().await;
    f.cleanup().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn runtime_cannot_forge_the_migration_flag_or_call_the_owner_function() {
    let f = Fixture::new().await;
    let role = format!("reverse_runtime_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'fixture-only-password'"
    ))
    .execute(&f.admin)
    .await
    .unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {role}")).execute(&f.pool).await.unwrap();
        sqlx::query(&format!("GRANT SELECT,INSERT,UPDATE,DELETE ON memories,learnings TO {role}")).execute(&f.pool).await.unwrap();
        let (database,corpus): (Uuid,Uuid) = sqlx::query_as("UPDATE knowledge_storage SET backend='fenced',generation=2,corpus_id=gen_random_uuid() RETURNING database_id,corpus_id").fetch_one(&f.pool).await.unwrap();
        let mut url = url::Url::parse(&f.url).unwrap(); url.set_username(&role).unwrap(); url.set_password(Some("fixture-only-password")).unwrap();
        let runtime = PgPoolOptions::new().max_connections(1).connect(url.as_str()).await.unwrap();
        let call = "SELECT public.ygg_knowledge_reverse_import($1,$2,2,'[]','[]')";
        assert!(sqlx::query(call).bind(database).bind(corpus).execute(&runtime).await.is_err());
        // Defense in depth: even an accidental EXECUTE grant cannot authorize it.
        sqlx::query(&format!("GRANT EXECUTE ON FUNCTION public.ygg_knowledge_reverse_import(UUID,UUID,BIGINT,JSONB,JSONB) TO {role}")).execute(&f.pool).await.unwrap();
        assert!(sqlx::query(call).bind(database).bind(corpus).execute(&runtime).await.is_err());
        let mut tx = runtime.begin().await.unwrap();
        // Temporary relations must not shadow the catalog owner lookup inside
        // the SECURITY DEFINER write fence.
        sqlx::query("CREATE TEMP TABLE pg_class (oid oid, relowner oid)").execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pg_temp.pg_class SELECT 'public.knowledge_storage'::regclass::oid, oid FROM pg_catalog.pg_roles WHERE rolname=session_user").execute(&mut *tx).await.unwrap();
        sqlx::query("SET LOCAL ygg.knowledge_reverse_import='on'").execute(&mut *tx).await.unwrap();
        assert!(sqlx::query("INSERT INTO memories(text) VALUES('forged bypass')").execute(&mut *tx).await.is_err()); tx.rollback().await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM memories").fetch_one(&f.pool).await.unwrap(), 0);
        runtime.close().await;
    }).catch_unwind().await;
    f.cleanup().await;
    // No role dependencies survive its isolated database.
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    sqlx::query(&format!("DROP ROLE {role}"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
