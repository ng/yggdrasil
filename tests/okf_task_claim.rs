#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use futures::FutureExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;
use ygg::{
    knowledge::{
        identity::{IdentityRegistry, RepoBinding},
        service::{Creation, KnowledgeService, RuleInput},
        store::KnowledgeStore,
    },
    models::{
        repo::RepoRepo,
        task::{TaskCreate, TaskKind, TaskRepo},
    },
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
        let database = format!("ygg_claim_{}", Uuid::new_v4().simple());
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
        sqlx::query(&format!("DROP DATABASE {}", self.database))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

async fn claim(f: &Fixture, root: &Path, repo: Uuid, paths: bool) -> String {
    let task = TaskRepo::new(&f.pool)
        .create(
            repo,
            None,
            TaskCreate {
                title: "Claim fixture",
                description: if paths {
                    "Edit src/one.rs and src/two.rs"
                } else {
                    "General work"
                },
                kind: TaskKind::Bug,
                ..TaskCreate::default()
            },
        )
        .await
        .unwrap();
    let out = okf::app(root, &root.join("repo"))
        .env("DATABASE_URL", &f.url)
        .args(["task", "claim", &task.task_id.to_string()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("claimed by fixture-agent"), "{text}");
    let status: String = sqlx::query_scalar("SELECT status::text FROM tasks WHERE task_id=$1")
        .bind(task.task_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(status, "in_progress");
    text
}

#[tokio::test]
async fn claims_use_database_task_scope_and_never_fall_back_after_selection() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut binding, _, _) = okf::fixture(root);
        let a = RepoRepo::new(&f.pool).register(None, "A", "a", None).await.unwrap().repo_id;
        let b = RepoRepo::new(&f.pool).register(None, "B", "b", None).await.unwrap().repo_id;
        let unknown = RepoRepo::new(&f.pool).register(None, "Unknown", "unknown", None).await.unwrap().repo_id;
        let database: Uuid = sqlx::query_scalar("SELECT database_id FROM public.knowledge_storage")
            .fetch_one(&f.pool).await.unwrap();
        binding.mappings.database_id = database;
        let registry = IdentityRegistry::open(&root.join("policy"), false).unwrap();
        let (mut identities, revision) = registry.read().unwrap();
        let portable_a = identities.repos[0].id;
        let portable_b = Uuid::new_v4();
        identities.repos[0].databases = BTreeMap::from([(database, [a].into())]);
        identities.repos.push(RepoBinding {
            id: portable_b, aliases: Default::default(), common_dirs: Default::default(),
            databases: BTreeMap::from([(database, [b].into())]),
        });
        registry.replace(&revision, &identities).unwrap();
        binding.mappings.repos = BTreeMap::from([(a, portable_a), (b, portable_b)]);
        binding.generation = 3;
        okf::select(root, &binding);
        let service = KnowledgeService::new(KnowledgeStore::open(&root.join("bundle"), false).unwrap(), registry, "portable-user".into()).unwrap();
        for (text, repo, file, rule, agent, kind, creation) in [
            ("CHECKOUT_A_ONLY", Some(portable_a), None, None, None, None, Creation::ManualActive),
            ("TASK_B_GENERAL", Some(portable_b), None, None, None, None, Creation::ManualActive),
            ("GLOBAL_GENERAL", None, None, None, None, None, Creation::ManualActive),
            ("MATCH_B_FILES", Some(portable_b), Some("src/*.rs"), None, Some("fixture-agent"), Some("bug"), Creation::ManualActive),
            ("WRONG_AGENT", Some(portable_b), None, None, Some("someone-else"), None, Creation::ManualActive),
            ("WRONG_KIND", Some(portable_b), None, None, None, Some("feature"), Creation::ManualActive),
            ("PENDING_RULE", Some(portable_b), None, None, None, None, Creation::Proposal),
            ("RULE_ID_ONLY", Some(portable_b), None, Some("rule-id"), None, None, Creation::ManualActive),
        ] {
            let scope_tags = [("agent", agent), ("kind", kind)].into_iter()
                .filter_map(|(k,v)| v.map(|v| (k.to_owned(), serde_json::json!(v)))).collect();
            service.create_rule(RuleInput { repo, text: text.into(), file_glob: file.map(str::to_owned),
                rule_id: rule.map(str::to_owned), scope_tags, ..Default::default() }, creation, chrono::Utc::now()).unwrap();
        }
        sqlx::query("INSERT INTO learnings(text, user_id) VALUES ('SQL_SENTINEL', 'legacy-user')")
            .execute(&f.pool).await.unwrap();
        // Even while SQL remains selected on the server, a local selection cannot
        // silently mix or fall back to it.
        let mismatch = claim(&f, root, b, true).await;
        assert!(!mismatch.contains("[ygg learning"), "{mismatch}");
        sqlx::query("UPDATE public.knowledge_storage SET backend='fenced', corpus_id=$1, generation=2")
            .bind(binding.mappings.corpus_id).execute(&f.pool).await.unwrap();
        sqlx::query("UPDATE public.knowledge_storage SET backend='okf', corpus_id=$1, generation=$2")
            .bind(binding.mappings.corpus_id).bind(binding.generation).execute(&f.pool).await.unwrap();
        let text = claim(&f, root, b, true).await;
        for expected in ["TASK_B_GENERAL", "GLOBAL_GENERAL", "MATCH_B_FILES", "RULE_ID_ONLY"] {
            assert_eq!(text.matches(expected).count(), 1, "{text}");
        }
        for excluded in ["CHECKOUT_A_ONLY", "WRONG_AGENT", "WRONG_KIND", "PENDING_RULE", "SQL_SENTINEL"] {
            assert!(!text.contains(excluded), "{text}");
        }
        let text = claim(&f, root, b, false).await;
        assert!(text.contains("TASK_B_GENERAL") && text.contains("GLOBAL_GENERAL"));
        assert!(!text.contains("MATCH_B_FILES") && !text.contains("RULE_ID_ONLY"));
        assert!(!claim(&f, root, unknown, true).await.contains("[ygg learning"));

        // Individually reject connected identity and generation mismatches.
        for (db, corpus, generation) in [
            (Uuid::new_v4(), binding.mappings.corpus_id, binding.generation),
            (database, Uuid::new_v4(), binding.generation),
            (database, binding.mappings.corpus_id, binding.generation + 1),
        ] {
            assert!(ygg::knowledge::guard::selected_transaction(&f.pool, db, corpus, generation).await.is_err());
        }
        binding.generation += 1;
        okf::select(root, &binding);
        assert!(!claim(&f, root, b, true).await.contains("[ygg learning"));
        binding.generation -= 1;
        // A coherent local mapping for a different database must also be refused.
        let registry = IdentityRegistry::open(&root.join("policy"), false).unwrap();
        let (mut policy, revision) = registry.read().unwrap();
        let other_database = Uuid::new_v4();
        for repo in &mut policy.repos {
            let ids = repo.databases.remove(&database).unwrap();
            repo.databases.insert(other_database, ids);
        }
        registry.replace(&revision, &policy).unwrap();
        binding.mappings.database_id = other_database;
        okf::select(root, &binding);
        assert!(!claim(&f, root, b, true).await.contains("[ygg learning"));
        let (_, revision) = registry.read().unwrap();
        for repo in &mut policy.repos {
            let ids = repo.databases.remove(&other_database).unwrap();
            repo.databases.insert(database, ids);
        }
        registry.replace(&revision, &policy).unwrap();
        binding.mappings.database_id = database;
        binding.phase = ygg::knowledge::runtime::Phase::Fenced;
        okf::select(root, &binding);
        assert!(!claim(&f, root, b, true).await.contains("[ygg learning"));
        std::fs::write(root.join("policy/runtime.json"), "broken").unwrap();
        assert!(!claim(&f, root, b, true).await.contains("[ygg learning"));
        binding.phase = ygg::knowledge::runtime::Phase::Okf;
        okf::select(root, &binding);

        // Connected readers lease the migration lock until their operation ends.
        let lease = ygg::knowledge::guard::selected_transaction(&f.pool, database,
            binding.mappings.corpus_id, binding.generation).await.unwrap();
        let mut writer = f.pool.begin().await.unwrap();
        let available: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(1497843531,1)")
            .fetch_one(&mut *writer).await.unwrap();
        assert!(!available);
        lease.rollback().await.unwrap();
        let available: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(1497843531,1)")
            .fetch_one(&mut *writer).await.unwrap();
        assert!(available);
        let start = std::time::Instant::now();
        assert!(!claim(&f, root, b, true).await.contains("[ygg learning"));
        assert!(start.elapsed() < std::time::Duration::from_secs(8));
        writer.rollback().await.unwrap();
        assert!(claim(&f, root, b, true).await.contains("MATCH_B_FILES"));

        let registry = IdentityRegistry::open(&root.join("policy"), false).unwrap();
        let (mut policy, revision) = registry.read().unwrap();
        policy.trusted = false;
        registry.replace(&revision, &policy).unwrap();
        assert!(!claim(&f, root, b, true).await.contains("[ygg learning"));
        let (_, revision) = registry.read().unwrap();
        policy.trusted = true;
        registry.replace(&revision, &policy).unwrap();
        let first = ygg::knowledge::guard::selected_transaction(&f.pool, database,
            binding.mappings.corpus_id, binding.generation).await.unwrap();
        let pool = f.pool.clone();
        let transition = tokio::spawn(async move {
            sqlx::query("UPDATE public.knowledge_storage SET generation=generation+1")
                .execute(&pool).await.unwrap();
        });
        // Wait for the trigger to be queued behind the existing reader.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))")
                    .fetch_one(&f.pool).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert!(!transition.is_finished());
        first.rollback().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), transition).await.unwrap().unwrap();
        binding.generation += 1;
        okf::select(root, &binding);
        assert!(claim(&f, root, b, true).await.contains("MATCH_B_FILES"));

        // An absent local selection still cannot read SQL once its DB is cut over.
        std::fs::remove_file(root.join("policy/runtime.json")).unwrap();
        assert!(!claim(&f, root, b, true).await.contains("SQL_SENTINEL"));
        okf::select(root, &binding);
        // Server-side fencing is independently enforced too.
        sqlx::query("UPDATE public.knowledge_storage SET generation=generation+1, backend='fenced'")
            .execute(&f.pool).await.unwrap();
        binding.generation += 1;
        okf::select(root, &binding);
        assert!(!claim(&f, root, b, true).await.contains("[ygg learning"));
        sqlx::query("UPDATE public.knowledge_storage SET generation=generation+1, backend='okf'")
            .execute(&f.pool).await.unwrap();
        binding.generation += 1;
        okf::select(root, &binding);
        assert!(claim(&f, root, b, true).await.contains("MATCH_B_FILES"));
        // A supported local client cannot consume a newer server protocol.
        sqlx::query("UPDATE public.knowledge_storage SET generation=generation+1, minimum_client=2147483647")
            .execute(&f.pool).await.unwrap();
        binding.generation += 1;
        okf::select(root, &binding);
        assert!(!claim(&f, root, b, true).await.contains("[ygg learning"));
    }).catch_unwind().await;
    f.cleanup().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
