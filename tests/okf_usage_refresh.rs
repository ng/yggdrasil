#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use futures::FutureExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::collections::BTreeMap;
use uuid::Uuid;
use ygg::knowledge::{
    identity::IdentityRegistry,
    legacy::{self, Usage},
    runtime::UsageSnapshot,
    service::{Creation, KnowledgeService, RuleInput},
    store::{ExpectedRevision, KnowledgeStore},
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
        let database = format!("ygg_usage_refresh_{}", Uuid::new_v4().simple());
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
async fn another_clients_totals_refresh_without_rewriting_documents_or_losing_baselines() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let temp = tempfile::tempdir().unwrap(); let root = temp.path();
        let (mut binding, _, _) = okf::fixture(root);
        binding.mappings.database_id = sqlx::query_scalar("SELECT database_id FROM public.knowledge_storage").fetch_one(&f.pool).await.unwrap();
        let registry = IdentityRegistry::open(&root.join("policy"),false).unwrap();
        let (mut identities, revision) = registry.read().unwrap();
        identities.repos[0].databases = BTreeMap::from([(binding.mappings.database_id,binding.mappings.repos.keys().copied().collect())]);
        registry.replace(&revision,&identities).unwrap();
        sqlx::query("UPDATE public.knowledge_storage SET backend='fenced',corpus_id=$1,generation=2").bind(binding.mappings.corpus_id).execute(&f.pool).await.unwrap();
        binding.generation=3;
        sqlx::query("UPDATE public.knowledge_storage SET backend='okf',generation=3").execute(&f.pool).await.unwrap();
        okf::select(root,&binding);
        let store = KnowledgeStore::open(&root.join("bundle"),false).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!("fixtures/knowledge/learnings.json")).unwrap();
        let mut source: ygg::models::learning::Learning = serde_json::from_value(fixture["rows"][0].clone()).unwrap();
        source.learning_id=Uuid::new_v4(); source.repo_id=None; source.applied_count=7;
        let (imported, baseline) = legacy::import_learning(&source,"legacy-user",&binding.mappings).unwrap();
        store.put(&imported,ExpectedRevision::Absent).unwrap();
        source.learning_id=Uuid::new_v4(); source.status="pending".into();
        let (pending, missing_baseline) = legacy::import_learning(&source,"legacy-user",&binding.mappings).unwrap();
        store.put(&pending,ExpectedRevision::Absent).unwrap();
        std::fs::write(root.join("policy/usage-baseline.json"),serde_json::to_vec(&UsageSnapshot { version:1,corpus_id:binding.mappings.corpus_id,totals:BTreeMap::from([(baseline.document_id,baseline.clone()),(missing_baseline.document_id,missing_baseline.clone())]) }).unwrap()).unwrap();
        let service = KnowledgeService::new(store,registry,"portable-user".into()).unwrap();
        let mut ids=Vec::new();
        for text in ["new rule","no observations","overflow"] {
            ids.push(service.create_rule(RuleInput{text:text.into(),..RuleInput::default()},Creation::ManualActive,chrono::Utc::now()).unwrap().key.id);
        }
        let telemetry=Telemetry::new(&f.pool);
        telemetry.seed(&baseline).await.unwrap();
        for _ in 0..2 { telemetry.record(binding.mappings.corpus_id,baseline.document_id,Uuid::new_v4(),chrono::Utc::now()).await.unwrap(); }
        for id in [ids[0],missing_baseline.document_id] { telemetry.record(binding.mappings.corpus_id,id,Uuid::new_v4(),chrono::Utc::now()).await.unwrap(); }
        telemetry.seed(&Usage { corpus_id:binding.mappings.corpus_id,document_id:ids[2],applied_count:i32::MAX,last_applied_at:None }).await.unwrap();
        telemetry.record(binding.mappings.corpus_id,ids[2],Uuid::new_v4(),chrono::Utc::now()).await.unwrap();
        let before: i64=sqlx::query_scalar("SELECT count(*) FROM public.knowledge_applications").fetch_one(&f.pool).await.unwrap();
        let bytes=std::fs::read(root.join("bundle/global/learnings").join(format!("{}.md",baseline.document_id))).unwrap();
        assert!(!root.join("policy/usage-snapshot.json").exists());
        let report=okf::json(okf::app(root,&root.join("repo")).env("DATABASE_URL",&f.url).args(["knowledge","refresh-usage","--json"]).output().unwrap());
        assert_eq!(report,serde_json::json!({"requested":5,"observed":2,"missing":1,"missing_baseline":1,"unrepresentable":1}));
        // Ordinary listing is still fully offline; it sees the other client's
        // committed usage and preserves the incomplete imported baseline.
        let rows=okf::json(okf::learn(root).args(["list","--all","--json"]).output().unwrap());
        let row=rows["results"].as_array().unwrap().iter().find(|r|r["learning_id"]==baseline.document_id.to_string()).unwrap();
        assert_eq!(row["applied_count"],9);
        let pending_rows=okf::json(okf::learn(root).args(["pending","--all","--json"]).output().unwrap());
        assert_eq!(pending_rows["results"][0]["applied_count"],7);
        assert_eq!(std::fs::read(root.join("bundle/global/learnings").join(format!("{}.md",baseline.document_id))).unwrap(),bytes);
        let after:i64=sqlx::query_scalar("SELECT count(*) FROM public.knowledge_applications").fetch_one(&f.pool).await.unwrap(); assert_eq!(before,after);
        let cached=std::fs::read(root.join("policy/usage-snapshot.json")).unwrap();
        sqlx::query("UPDATE public.knowledge_usage SET observed_count=0 WHERE corpus_id=$1 AND document_id=$2")
            .bind(binding.mappings.corpus_id).bind(baseline.document_id).execute(&f.pool).await.unwrap();
        okf::json(okf::app(root, &root.join("repo")).env("DATABASE_URL", &f.url)
            .args(["knowledge", "refresh-usage", "--json"]).output().unwrap());
        assert_eq!(std::fs::read(root.join("policy/usage-snapshot.json")).unwrap(), cached);
        let mut blocker = f.pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)").execute(&mut *blocker).await.unwrap();
        let started = std::time::Instant::now();
        let timed = okf::app(root, &root.join("repo")).env("DATABASE_URL", &f.url)
            .args(["knowledge", "refresh-usage", "--json"]).output().unwrap();
        assert!(!timed.status.success());
        assert!(String::from_utf8_lossy(&timed.stderr).contains("timed out"));
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
        assert_eq!(std::fs::read(root.join("policy/usage-snapshot.json")).unwrap(), cached);
        blocker.rollback().await.unwrap();

        sqlx::query("UPDATE public.knowledge_storage SET generation=generation+1").execute(&f.pool).await.unwrap();
        let failed=okf::app(root,&root.join("repo")).env("DATABASE_URL",&f.url).args(["knowledge","refresh-usage","--json"]).output().unwrap();
        assert!(!failed.status.success()); assert!(failed.stdout.is_empty());
        assert_eq!(std::fs::read(root.join("policy/usage-snapshot.json")).unwrap(),cached);
        let failed=okf::app(root,&root.join("repo")).env("DATABASE_URL","postgres://127.0.0.1:1/unavailable").args(["knowledge","refresh-usage","--json"]).output().unwrap();
        assert!(!failed.status.success()); assert_eq!(std::fs::read(root.join("policy/usage-snapshot.json")).unwrap(),cached);
    }).catch_unwind().await;
    f.cleanup().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
