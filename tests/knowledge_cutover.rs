#![cfg(any(target_os = "macos", target_os = "linux"))]
use futures::FutureExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::BTreeMap, path::PathBuf, process::Command};
use uuid::Uuid;
use ygg::knowledge::{
    clients,
    cutover::PrivateJournal,
    export,
    forward::{self, Outcome},
    identity::IdentityRegistry,
    inventory,
    legacy::Mappings,
    rollback::RecoveryPaths,
    runtime::{Binding, Phase},
    store::KnowledgeStore,
};

struct Fixture {
    admin: PgPool,
    pool: PgPool,
    database: String,
    temp: tempfile::TempDir,
    paths: RecoveryPaths,
    manifest: export::Manifest,
}
impl Fixture {
    async fn new() -> Self {
        let base = std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&base)
            .await
            .unwrap();
        let database = format!("ygg_cutover_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&base).unwrap();
        url.set_path(&format!("/{database}"));
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(url.as_str())
            .await
            .unwrap();
        ygg::db::run_migrations(&pool).await.unwrap();
        clients::register(&mut pool.acquire().await.unwrap())
            .await
            .unwrap();
        sqlx::query("INSERT INTO memories(text,user_id) VALUES('original note','alice')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO learnings(text,user_id,status,source,applied_count) VALUES('pending rule','alice','pending','proposed',7)").execute(&pool).await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let paths = RecoveryPaths {
            corpus: root.join("published"),
            policy: root.join("policy"),
            corpus_archive: root.join("corpus-backup"),
            policy_archive: root.join("policy-backup"),
        };
        let registry = IdentityRegistry::open(&paths.policy, true).unwrap();
        let id = registry.initialize(true).unwrap();
        let source = inventory::assess(&pool, None).await.unwrap().source;
        let mappings = Mappings {
            database_id: source.database_id,
            corpus_id: id.corpus_id,
            repos: BTreeMap::new(),
            users: BTreeMap::from([("alice".into(), "owner-λ".into())]),
        };
        sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=2,corpus_id=$1")
            .bind(id.corpus_id)
            .execute(&pool)
            .await
            .unwrap();
        let stage = root.join("stage");
        export::stage(&pool, &mappings, &stage).await.unwrap();
        let manifest = export::publish(
            &pool,
            &stage,
            &root.join("publication-backup"),
            &paths.corpus,
        )
        .await
        .unwrap();
        let binding = Binding {
            version: 1,
            minimum_client: 1,
            generation: 2,
            phase: Phase::Fenced,
            bundle: paths.corpus.clone(),
            mappings,
            agents: BTreeMap::new(),
        };
        std::fs::write(
            paths.policy.join("runtime.json"),
            serde_json::to_vec(&binding).unwrap(),
        )
        .unwrap();
        Self {
            admin,
            pool,
            database,
            temp,
            paths,
            manifest,
        }
    }
    fn directory(&self) -> PathBuf {
        self.temp.path().canonicalize().unwrap().join("journal")
    }
    fn manifest(&self) -> export::Manifest {
        serde_json::from_value(serde_json::to_value(&self.manifest).unwrap()).unwrap()
    }
    fn prepare(&self) -> PrivateJournal {
        let corpus = KnowledgeStore::open(&self.paths.corpus, false).unwrap();
        let policy = KnowledgeStore::open(&self.paths.policy, false).unwrap();
        let pair = corpus
            .backup_pair_retained(
                &policy,
                &self.paths.corpus_archive,
                &self.paths.policy_archive,
            )
            .unwrap();
        let journal = PrivateJournal::prepare(
            &self.directory(),
            self.manifest(),
            &pair,
            self.paths.clone(),
        )
        .unwrap();
        // Busy prepare must not wait while holding source leases.
        assert!(
            PrivateJournal::prepare(
                &self.directory(),
                self.manifest(),
                &pair,
                self.paths.clone()
            )
            .is_err()
        );
        let operation = journal.operation();
        drop(journal);
        let repeated = PrivateJournal::prepare(
            &self.directory(),
            self.manifest(),
            &pair,
            self.paths.clone(),
        )
        .unwrap();
        assert_eq!(operation, repeated.operation());
        repeated
    }
    fn phase(&self) -> Phase {
        serde_json::from_slice::<Binding>(
            &std::fs::read(self.paths.policy.join("runtime.json")).unwrap(),
        )
        .unwrap()
        .phase
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
async fn sql_commit_gap_resumes_local_activation_and_preserves_later_offline_writes() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let journal = f.prepare();
        let operation = journal.operation();
        drop(journal);
        // Simulate losing the process after the actual SQL commit, before file publication.
        let mut tx = f.pool.begin().await.unwrap();
        assert_eq!(
            forward::activate_on(&mut tx, operation, &f.manifest)
                .await
                .unwrap(),
            Outcome::Activated
        );
        tx.commit().await.unwrap();
        assert!(f.phase() == Phase::Fenced);
        // A kill can leave even an incomplete UTF-8 code point in the temporary.
        let mut target: Binding =
            serde_json::from_slice(&std::fs::read(f.paths.policy.join("runtime.json")).unwrap())
                .unwrap();
        target.phase = Phase::Okf;
        target.generation = 3;
        let target = serde_json::to_vec(&target).unwrap();
        let partial_end = target
            .windows(2)
            .position(|bytes| bytes == "λ".as_bytes())
            .unwrap()
            + 1;
        let temporary = f.paths.policy.join(format!(".cutover-{operation}.tmp"));
        std::fs::write(&temporary, &target[..partial_end]).unwrap();

        let journal = PrivateJournal::open(&f.directory()).unwrap();
        assert_eq!(
            journal.activate(&f.pool).await.unwrap(),
            Outcome::PreviouslyActivated
        );
        assert!(f.phase() == Phase::Okf);
        let output = Command::new(env!("CARGO_BIN_EXE_ygg"))
            .args([
                "remember",
                "post-cutover offline note",
                "--global",
                "--json",
            ])
            .env("YGG_CONFIG_DIR", f.temp.path().join("config"))
            .env("YGG_DATA_DIR", f.temp.path().join("data"))
            .env("YGG_KNOWLEDGE_DIR", &f.paths.corpus)
            .env("YGG_KNOWLEDGE_POLICY_DIR", &f.paths.policy)
            .env("YGG_USER", "alice")
            .env("YGG_DB_MODE", "invalid")
            .env("DATABASE_URL", "invalid-secret")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let snapshot = KnowledgeStore::open(&f.paths.corpus, false)
            .unwrap()
            .snapshot();
        assert!(
            snapshot
                .documents
                .iter()
                .any(|d| d.document.body == "post-cutover offline note")
        );
        let revisions: Vec<_> = snapshot
            .documents
            .iter()
            .map(|d| (&d.key, &d.revision))
            .collect();
        // Resume after local publication must preserve every acknowledged new byte.
        drop(journal);
        assert_eq!(
            PrivateJournal::open(&f.directory())
                .unwrap()
                .activate(&f.pool)
                .await
                .unwrap(),
            Outcome::PreviouslyActivated
        );
        let current = KnowledgeStore::open(&f.paths.corpus, false)
            .unwrap()
            .snapshot();
        assert_eq!(
            revisions,
            current
                .documents
                .iter()
                .map(|d| (&d.key, &d.revision))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM knowledge_forward_receipts")
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM learnings WHERE status='pending'")
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            1
        );
        assert!(
            sqlx::query("INSERT INTO memories(text) VALUES('legacy write')")
                .execute(&f.pool)
                .await
                .is_err()
        );
        sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=4")
            .execute(&f.pool)
            .await
            .unwrap();
        assert!(
            PrivateJournal::open(&f.directory())
                .unwrap()
                .activate(&f.pool)
                .await
                .is_err()
        );
    })
    .catch_unwind()
    .await;
    f.cleanup().await;
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

#[tokio::test]
async fn conflicts_and_relocation_cannot_publish_or_infer_activation_from_local_bytes() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let journal = f.prepare();
        drop(journal);
        let moved = f.temp.path().join("moved-journal");
        std::fs::rename(f.directory(), &moved).unwrap();
        assert!(PrivateJournal::open(&moved).is_err());
        std::fs::rename(&moved, f.directory()).unwrap();
        let journal = PrivateJournal::open(&f.directory()).unwrap();
        let file = f.paths.policy.join("runtime.json");
        let original = std::fs::read(&file).unwrap();
        let mut binding: Binding = serde_json::from_slice(&original).unwrap();
        // Even exact desired bytes cannot supply the missing SQL receipt.
        binding.phase = Phase::Okf;
        binding.generation = 3;
        std::fs::write(&file, serde_json::to_vec(&binding).unwrap()).unwrap();
        assert!(
            journal
                .activate(&f.pool)
                .await
                .unwrap_err()
                .to_string()
                .contains("no SQL activation receipt")
        );
        std::fs::write(&file, &original).unwrap();
        let temporary = f
            .paths
            .policy
            .join(format!(".cutover-{}.tmp", journal.operation()));
        std::fs::write(&temporary, b"foreign bytes").unwrap();
        assert!(journal.activate(&f.pool).await.is_err());
        assert_eq!(std::fs::read(&temporary).unwrap(), b"foreign bytes");
        std::fs::remove_file(&temporary).unwrap();
        let identity = f.paths.policy.join("identity.json");
        let original_identity = std::fs::read(&identity).unwrap();
        std::fs::write(&identity, b"{}").unwrap();
        std::fs::write(&temporary, b"{").unwrap();
        assert!(journal.activate(&f.pool).await.is_err());
        assert_eq!(std::fs::read(&temporary).unwrap(), b"{");
        std::fs::remove_file(&temporary).unwrap();
        std::fs::write(&identity, &original_identity).unwrap();
        let store = KnowledgeStore::open(&f.paths.corpus, false).unwrap();
        let doc = store.snapshot().documents.remove(0);
        let path = f.paths.corpus.join(doc.key.relative_path());
        let content = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"independent edit").unwrap();
        assert!(journal.activate(&f.pool).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"independent edit");
        std::fs::write(&path, &content).unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT backend FROM knowledge_storage")
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            "fenced"
        );
        assert_eq!(journal.activate(&f.pool).await.unwrap(), Outcome::Activated);
        assert!(f.phase() == Phase::Okf);
    })
    .catch_unwind()
    .await;
    f.cleanup().await;
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}
