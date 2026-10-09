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
        let receipt_call = "SELECT public.ygg_knowledge_reverse_apply_once(gen_random_uuid(),$1,$2,2,'{}','[]','[]')";
        assert!(sqlx::query(receipt_call).bind(database).bind(corpus).execute(&runtime).await.is_err());
        sqlx::query(&format!("GRANT EXECUTE ON FUNCTION public.ygg_knowledge_reverse_apply_once(UUID,UUID,UUID,BIGINT,JSONB,JSONB,JSONB) TO {role}")).execute(&f.pool).await.unwrap();
        let error = sqlx::query(receipt_call).bind(database).bind(corpus).execute(&runtime).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("42501"));
        sqlx::query(&format!("GRANT ALL ON knowledge_reverse_receipts TO {role}")).execute(&f.pool).await.unwrap();
        let error = sqlx::query("INSERT INTO knowledge_reverse_receipts(operation_id,database_id,corpus_id,fenced_generation,request_sha256,notes_sha256,rules_sha256,evidence) VALUES(gen_random_uuid(),$1,$2,2,repeat('0',64),repeat('0',64),repeat('0',64),'{}')")
            .bind(database).bind(corpus).execute(&runtime).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("42501"));
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

#[tokio::test]
async fn capture_uses_frozen_database_totals_and_rejects_missing_or_conflicting_baselines() {
    use ygg::knowledge::telemetry::Telemetry;
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let rule = Uuid::new_v4();
        sqlx::query("INSERT INTO learnings(learning_id,text,user_id,status,source,applied_count) VALUES($1,'original','alice','pending','proposed',5)")
            .bind(rule).execute(&f.pool).await.unwrap();
        let source = inventory::assess(&f.pool, None).await.unwrap().source;
        let mappings = Mappings { database_id: source.database_id, corpus_id: Uuid::new_v4(), repos: BTreeMap::new(), users: BTreeMap::from([("alice".into(), "owner".into())]) };
        sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=2,corpus_id=$1").bind(mappings.corpus_id).execute(&f.pool).await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("bundle");
        let manifest = export::stage(&f.pool, &mappings, &stage).await.unwrap();
        let store = KnowledgeStore::open(&stage, false).unwrap();
        // A genuinely new pending rule has no migration baseline or observations.
        let mut new = store.snapshot().documents.remove(0).document;
        let mut profile = new.profile().unwrap().unwrap();
        let added = Uuid::new_v4();
        profile.id = added;
        profile.extra.remove("legacy");
        profile.legacy_repo_id = None;
        new.set_profile(&profile).unwrap();
        store.put(&new, ExpectedRevision::Absent).unwrap();
        let snapshot = store.snapshot();
        sqlx::query("UPDATE knowledge_storage SET backend='okf',generation=3").execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(reverse::capture_on(&mut tx, &manifest, &snapshot, 4).await.is_err());
        tx.rollback().await.unwrap();
        sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=4").execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(reverse::capture_on(&mut tx, &manifest, &snapshot, 4).await.is_err());
        tx.rollback().await.unwrap();
        let baseline = manifest.entries[0].usage.as_ref().unwrap();
        let telemetry = Telemetry::new(&f.pool);
        telemetry.seed(baseline).await.unwrap();
        let at = Utc::now().trunc_subsecs(6);
        for _ in 0..2 { telemetry.record(mappings.corpus_id, rule, Uuid::new_v4(), at).await.unwrap(); }
        let mut tx = f.pool.begin().await.unwrap();
        let policy = KnowledgeStore::open(&temp.path().join("policy"), true).unwrap();
        let recovery = store.backup_pair_retained(&policy, &temp.path().join("recovery-corpus"), &temp.path().join("recovery-policy")).unwrap();
        let candidate = reverse::capture_recovery_on(&mut tx, &manifest, &recovery, 4).await.unwrap();
        let old = candidate.learnings.iter().find(|r| r["learning_id"] == rule.to_string()).unwrap();
        assert_eq!(old["applied_count"], 7);
        assert_eq!(serde_json::from_value::<chrono::DateTime<Utc>>(old["last_applied_at"].clone()).unwrap(), at);
        let new = candidate.learnings.iter().find(|r| r["learning_id"] == added.to_string()).unwrap();
        assert_eq!(new["applied_count"], 0);
        assert!(new["last_applied_at"].is_null());
        // The same lock protects existing rows and inserts for absent rules.
        let mut other = f.pool.acquire().await.unwrap();
        sqlx::query("SET statement_timeout='200ms'").execute(&mut *other).await.unwrap();
        for id in [rule, added] {
            let error = sqlx::query("INSERT INTO knowledge_usage(corpus_id,document_id,observed_count) VALUES($1,$2,1) ON CONFLICT(corpus_id,document_id) DO UPDATE SET observed_count=knowledge_usage.observed_count+1")
                .bind(mappings.corpus_id).bind(id).execute(&mut *other).await.unwrap_err();
            assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("57014"));
        }
        sqlx::query("RESET statement_timeout").execute(&mut *other).await.unwrap();
        drop(other);
        reverse::apply_on(&mut tx, &candidate, 4).await.unwrap();
        recovery.verify_sources().unwrap();
        tx.commit().await.unwrap();
        drop(recovery);
        assert_eq!(sqlx::query_scalar::<_,i32>("SELECT applied_count FROM learnings WHERE learning_id=$1").bind(rule).fetch_one(&f.pool).await.unwrap(), 7);
        // The same rollback rows can be captured from a pinned authoritative Git tree.
        let remote = temp.path().join("remote.git");
        let seed = temp.path().join("seed");
        let git = |cwd: &std::path::Path, args: &[&str]| {
            let output = std::process::Command::new("git").arg("-C").arg(cwd)
                .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"])
                .args(args).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        };
        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(temp.path(), &["init", "-b", "knowledge", seed.to_str().unwrap()]);
        git(&seed, &["commit", "--allow-empty", "-m", "initial"]);
        git(&seed, &["push", remote.to_str().unwrap(), "HEAD:refs/heads/knowledge"]);
        let shared_root = temp.path().join("shared");
        let transport = ygg::knowledge::shared::SharedGit::open(&shared_root, ygg::knowledge::shared::Config {
            version: 1, remote: remote.to_str().unwrap().into(), branch: "knowledge".into()
        }).unwrap();
        let changes: Vec<_> = snapshot.documents.iter().map(|d| ygg::knowledge::shared::Change {
            path: d.key.relative_path().to_str().unwrap().into(), expected: None,
            replacement: Some(d.document.serialize().unwrap().into_bytes())
        }).collect();
        let published = transport.change(&changes).unwrap();
        let shared = KnowledgeStore::open(&shared_root, false).unwrap();
        let saved = shared.backup_pair_retained(&policy, &temp.path().join("shared-archive"), &temp.path().join("shared-policy")).unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        let captured = reverse::capture_shared_recovery_on(&mut tx, &manifest, &transport, &saved, 4).await.unwrap();
        assert_eq!(captured.commit, published.commit);
        assert_eq!(serde_json::to_value(&captured.candidate).unwrap(), serde_json::to_value(&candidate).unwrap());
        reverse::apply_on(&mut tx, &captured.candidate, 4).await.unwrap();
        transport.verify_recovery(&saved, &captured.commit).unwrap();
        tx.rollback().await.unwrap();
        drop(saved);
        // Table locks are released on commit, and overflow is rejected instead of clamped.
        sqlx::query("UPDATE knowledge_usage SET observed_count=2147483647 WHERE corpus_id=$1 AND document_id=$2").bind(mappings.corpus_id).bind(rule).execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(reverse::capture_on(&mut tx, &manifest, &snapshot, 4).await.is_err());
        tx.rollback().await.unwrap();
        sqlx::query("UPDATE knowledge_usage SET observed_count=2 WHERE corpus_id=$1 AND document_id=$2").bind(mappings.corpus_id).bind(rule).execute(&f.pool).await.unwrap();
        sqlx::query("INSERT INTO knowledge_usage(corpus_id,document_id,imported_count) VALUES($1,$2,0)").bind(mappings.corpus_id).bind(added).execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(reverse::capture_on(&mut tx, &manifest, &snapshot, 4).await.is_err());
        tx.rollback().await.unwrap();
        sqlx::query("DELETE FROM knowledge_usage WHERE corpus_id=$1 AND document_id=$2").bind(mappings.corpus_id).bind(added).execute(&f.pool).await.unwrap();
        sqlx::query("UPDATE knowledge_usage SET imported_count=6 WHERE corpus_id=$1 AND document_id=$2").bind(mappings.corpus_id).bind(rule).execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(reverse::capture_on(&mut tx, &manifest, &snapshot, 4).await.is_err());
        tx.rollback().await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").execute(&mut *tx).await.unwrap();
        assert!(reverse::capture_on(&mut tx, &manifest, &snapshot, 4).await.is_err());
        tx.rollback().await.unwrap();
    }).catch_unwind().await;
    f.cleanup().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn receipts_commit_atomically_and_verify_retries_without_reapplying_rows() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let note = Uuid::new_v4();
        sqlx::query("INSERT INTO memories(memory_id,text,user_id) VALUES($1,'before','alice')")
            .bind(note)
            .execute(&f.pool)
            .await
            .unwrap();
        let source = inventory::assess(&f.pool, None).await.unwrap().source;
        let mappings = Mappings {
            database_id: source.database_id,
            corpus_id: Uuid::new_v4(),
            repos: BTreeMap::new(),
            users: BTreeMap::from([("alice".into(), "owner".into())]),
        };
        sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=2,corpus_id=$1")
            .bind(mappings.corpus_id)
            .execute(&f.pool)
            .await
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("bundle");
        let manifest = export::stage(&f.pool, &mappings, &stage).await.unwrap();
        let store = KnowledgeStore::open(&stage, false).unwrap();
        let old = store.snapshot().documents.remove(0);
        let mut doc = old.document;
        doc.body = "current acknowledged content".into();
        store
            .put(&doc, ExpectedRevision::Digest(&old.revision))
            .unwrap();
        sqlx::query("UPDATE knowledge_storage SET backend='okf',generation=3")
            .execute(&f.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=4")
            .execute(&f.pool)
            .await
            .unwrap();
        let policy = KnowledgeStore::open(&temp.path().join("policy"), true).unwrap();
        let recovery = store
            .backup_pair_retained(
                &policy,
                &temp.path().join("archive"),
                &temp.path().join("policy-archive"),
            )
            .unwrap();
        let operation = Uuid::new_v4();
        let mut tx = f.pool.begin().await.unwrap();
        let candidate = reverse::capture_recovery_on(&mut tx, &manifest, &recovery, 4)
            .await
            .unwrap();
        let evidence = reverse::RecoveryEvidence::new(&candidate, &recovery, None).unwrap();
        assert_eq!(
            reverse::apply_once_on(&mut tx, operation, &candidate, &evidence, 4)
                .await
                .unwrap(),
            reverse::ApplyOutcome::Applied
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT text FROM memories WHERE memory_id=$1")
                .bind(note)
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            "before"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM knowledge_reverse_receipts")
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            0
        );
        let mut tx = f.pool.begin().await.unwrap();
        assert!(
            reverse::apply_once_on(&mut tx, operation, &candidate, &evidence, 5)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert_eq!(
            reverse::apply_once_on(&mut tx, operation, &candidate, &evidence, 4)
                .await
                .unwrap(),
            reverse::ApplyOutcome::Applied
        );
        recovery.verify_sources().unwrap();
        // A second connection cannot treat an uncommitted receipt as absent:
        // it waits for the original migration transaction to resolve.
        let resumed = PgPoolOptions::new()
            .max_connections(1)
            .connect(&f.url)
            .await
            .unwrap();
        let mut resumed_tx = resumed.begin().await.unwrap();
        let mut pending = Box::pin(reverse::apply_once_on(
            &mut resumed_tx,
            operation,
            &candidate,
            &evidence,
            4,
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), pending.as_mut())
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        assert_eq!(
            pending.await.unwrap(),
            reverse::ApplyOutcome::PreviouslyApplied
        );
        resumed_tx.commit().await.unwrap();
        resumed.close().await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM knowledge_reverse_receipts")
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            1
        );
        for statement in [
            "UPDATE knowledge_reverse_receipts SET notes_sha256=repeat('0',64)",
            "DELETE FROM knowledge_reverse_receipts",
            "TRUNCATE knowledge_reverse_receipts",
        ] {
            assert!(sqlx::query(statement).execute(&f.pool).await.is_err());
        }
        let mut conflict = evidence.clone();
        conflict.policy_revision = "0".repeat(64);
        let mut tx = f.pool.begin().await.unwrap();
        assert!(
            reverse::apply_once_on(&mut tx, operation, &candidate, &conflict, 4)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            reverse::apply_once_on(&mut tx, operation, &candidate, &evidence, 4)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        // An owner edit after the receipt must be detected and preserved.
        let mut tx = f.pool.begin().await.unwrap();
        sqlx::query("SET LOCAL ygg.knowledge_reverse_import='on'")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE memories SET text='independent owner edit' WHERE memory_id=$1")
            .bind(note)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(
            reverse::apply_once_on(&mut tx, operation, &candidate, &evidence, 4)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT text FROM memories WHERE memory_id=$1")
                .bind(note)
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            "independent owner edit"
        );
    })
    .catch_unwind()
    .await;
    f.cleanup().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
