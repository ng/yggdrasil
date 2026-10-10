#![cfg(any(target_os = "macos", target_os = "linux"))]
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Command, Output, Stdio},
};
use uuid::Uuid;
use ygg::knowledge::{
    clients,
    identity::Identities,
    legacy::Mappings,
    migration::{Host, Plan},
};

struct Server {
    bin: PathBuf,
    data: PathBuf,
    _temp: tempfile::TempDir,
    url: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new(self.bin.join("pg_ctl"))
            .arg("-D")
            .arg(&self.data)
            .args(["stop", "-m", "fast", "-w", "-t", "10"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
impl Server {
    fn new() -> Self {
        let bin = PathBuf::from(
            std::env::var("YGG_TEST_PG_BIN").expect("native PostgreSQL tools required"),
        );
        let temp = tempfile::Builder::new()
            .prefix("ygg-km-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        let data = root.join("pg");
        success(
            Command::new(bin.join("initdb"))
                .arg("-D")
                .arg(&data)
                .args([
                    "-U",
                    "postgres",
                    "-A",
                    "trust",
                    "--no-locale",
                    "--encoding=UTF8",
                ])
                .output()
                .unwrap(),
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let result = Self {
            bin,
            data,
            _temp: temp,
            url: format!("postgres://postgres@127.0.0.1:{port}/postgres"),
        };
        success(
            Command::new(result.bin.join("pg_ctl"))
                .arg("-D")
                .arg(&result.data)
                .arg("-l")
                .arg(root.join("server.log"))
                .arg("-o")
                .arg(format!("-k {} -p {port} -h 127.0.0.1", root.display()))
                .args(["-w", "-t", "30", "start"])
                .output()
                .unwrap(),
        );
        result
    }
}
struct Fixture {
    pool: PgPool,
    temp: tempfile::TempDir,
    env: BTreeMap<String, String>,
    plan: Plan,
    journal: PathBuf,
}
impl Fixture {
    async fn new(server: &Server) -> Self {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&server.url)
            .await
            .unwrap();
        let name = format!("km_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        let mut url = url::Url::parse(&server.url).unwrap();
        url.set_path(&name);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(url.as_str())
            .await
            .unwrap();
        ygg::db::run_migrations(&pool).await.unwrap();
        clients::register(&mut pool.acquire().await.unwrap())
            .await
            .unwrap();
        sqlx::query("INSERT INTO memories(text,user_id) VALUES('source note','alice')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO learnings(text,user_id,status,source,applied_count) VALUES('pending rule','alice','pending','proposed',7)").execute(&pool).await.unwrap();
        let database_id: Uuid = sqlx::query_scalar("SELECT database_id FROM knowledge_storage")
            .fetch_one(&pool)
            .await
            .unwrap();
        let corpus_id = Uuid::new_v4();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let env = BTreeMap::from([
            ("YGG_DB_MODE".into(), "external".into()),
            ("DATABASE_URL".into(), url.to_string()),
            (
                "YGG_CONFIG_DIR".into(),
                root.join("config").display().to_string(),
            ),
            (
                "YGG_DATA_DIR".into(),
                root.join("data").display().to_string(),
            ),
            (
                "YGG_KNOWLEDGE_DIR".into(),
                root.join("corpus").display().to_string(),
            ),
            (
                "YGG_KNOWLEDGE_POLICY_DIR".into(),
                root.join("policy").display().to_string(),
            ),
            ("YGG_USER".into(), "alice".into()),
        ]);
        let plan = Plan {
            version: 1,
            transport: "private".into(),
            source_generation: 1,
            mappings: Mappings {
                database_id,
                corpus_id,
                repos: BTreeMap::new(),
                users: BTreeMap::from([("alice".into(), "alice".into())]),
            },
            identities: Identities {
                version: 1,
                corpus_id,
                trusted: true,
                approval_leads: Default::default(),
                repos: vec![],
            },
            agents: BTreeMap::new(),
            execution_host: "fixture".into(),
            all_participating_hosts_listed: true,
            hosts: vec![Host {
                name: "fixture".into(),
                protocol: 1,
                knowledge_writers_stopped: true,
                external_editors_stopped: true,
                schema_changes_stopped: true,
                session_preserving_endpoint: true,
            }],
        };
        Self {
            pool,
            temp,
            env,
            plan,
            journal: root.join("journal"),
        }
    }
    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_ygg"));
        c.env_remove("YGG_DATABASE_OWNER_URL")
            .envs(&self.env)
            .current_dir(self.temp.path());
        c
    }
    async fn participant(&self, bytes: &[u8]) -> Output {
        use tokio::io::AsyncWriteExt;
        let mut child = self
            .command()
            .args(["knowledge", "fleet-participant"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(bytes).await.unwrap();
        input.shutdown().await.unwrap();
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
    }
    async fn migrate(&self, server: &Server, abort: bool) -> Output {
        let plan = self.temp.path().join("plan.json");
        std::fs::write(&plan, serde_json::to_vec(&self.plan).unwrap()).unwrap();
        let mut c = self.command();
        c.args(["knowledge", "migrate", "--plan"])
            .arg(plan)
            .arg("--journal")
            .arg(&self.journal)
            .arg("--pg-bin")
            .arg(&server.bin)
            .arg("--json");
        if abort {
            c.arg("--abort");
        }
        c.output().await.unwrap()
    }
    async fn marker(&self) -> (i64, String) {
        sqlx::query_as("SELECT generation,backend FROM knowledge_storage")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_private_migration_execution_resume_and_abort() {
    let server = Server::new();
    let f = Fixture::new(&server).await;
    let output = success(f.migrate(&server, false).await);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"], "okf");
    assert_eq!(f.marker().await, (3, "okf".into()));
    ygg::db::deployment_backup::verify(&f.journal.join("source-backup")).unwrap();
    assert!(
        sqlx::query("UPDATE memories SET text='stale SQL write'")
            .execute(&f.pool)
            .await
            .is_err()
    );
    success(
        f.command()
            .args(["remember", "offline after cutover", "--global"])
            .env("YGG_DB_MODE", "invalid")
            .env("DATABASE_URL", "unusable")
            .output()
            .await
            .unwrap(),
    );
    success(f.migrate(&server, false).await);
    let listed = success(
        f.command()
            .args(["remember", "--list", "--all"])
            .env("YGG_DB_MODE", "invalid")
            .output()
            .await
            .unwrap(),
    );
    assert!(String::from_utf8_lossy(&listed.stdout).contains("offline after cutover"));
    assert!(!f.migrate(&server, true).await.status.success());
    assert_eq!(f.marker().await, (3, "okf".into()));
    f.pool.close().await;

    // An invalid target causes a failure after the durable SQL fence. Abort must
    // preserve its bytes, restore SQL selection, and tolerate subsequent SQL edits.
    let f = Fixture::new(&server).await;
    let corpus = PathBuf::from(&f.env["YGG_KNOWLEDGE_DIR"]);
    let store = ygg::knowledge::store::KnowledgeStore::open(&corpus, true).unwrap();
    drop(store);
    std::fs::write(corpus.join("independent.txt"), "retain this file").unwrap();
    // Existing corpus requires existing explicit identity policy for the backup.
    let registry = ygg::knowledge::identity::IdentityRegistry::open(
        std::path::Path::new(&f.env["YGG_KNOWLEDGE_POLICY_DIR"]),
        true,
    )
    .unwrap();
    let identity = registry.initialize(true).unwrap();
    let mut f = f;
    f.plan.identities = identity.clone();
    f.plan.mappings.corpus_id = identity.corpus_id;
    assert!(!f.migrate(&server, false).await.status.success());
    assert_eq!(f.marker().await, (2, "fenced".into()));
    success(f.migrate(&server, true).await);
    assert_eq!(f.marker().await, (3, "sql".into()));
    sqlx::query("UPDATE memories SET text='after abort'")
        .execute(&f.pool)
        .await
        .unwrap();
    success(f.migrate(&server, true).await);
    assert_eq!(
        std::fs::read_to_string(corpus.join("independent.txt")).unwrap(),
        "retain this file"
    );
    assert!(!f.migrate(&server, false).await.status.success());
    f.pool.close().await;

    // Prepare a real source backup, then stop the CLI at a held local selection
    // lease after SQL fencing/publication. Killing the process must release its
    // journal lease; retry finishes the same operation and reuses the same dump.
    let f = Fixture::new(&server).await;
    let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
    let plan_copy = serde_json::from_value(serde_json::to_value(&f.plan).unwrap()).unwrap();
    drop(ygg::knowledge::migration::Journal::prepare(&f.journal, plan_copy, &config).unwrap());
    ygg::db::deployment_backup::create(
        &config,
        &f.journal.join("source-backup"),
        Some(&server.bin),
        None,
    )
    .await
    .unwrap();
    let before = std::fs::read(f.journal.join("source-backup/backup.json")).unwrap();
    let policy =
        ygg::knowledge::store::KnowledgeStore::open(&config.knowledge_policy_dir, true).unwrap();
    use std::os::unix::fs::OpenOptionsExt;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(config.knowledge_policy_dir.join(".selection.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&held).unwrap();
    let plan = f.temp.path().join("plan.json");
    std::fs::write(&plan, serde_json::to_vec(&f.plan).unwrap()).unwrap();
    let mut child = f
        .command()
        .args(["knowledge", "migrate", "--plan"])
        .arg(&plan)
        .arg("--journal")
        .arg(&f.journal)
        .arg("--pg-bin")
        .arg(&server.bin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "migration exited before blocked selection"
            );
            if f.marker().await == (2, "fenced".into())
                && config.knowledge_dir.join(".export-complete.json").exists()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    drop(held);
    drop(policy);
    assert_eq!(f.marker().await, (2, "fenced".into()));
    success(f.migrate(&server, false).await);
    assert_eq!(f.marker().await, (3, "okf".into()));
    assert_eq!(
        std::fs::read(f.journal.join("source-backup/backup.json")).unwrap(),
        before
    );
    f.pool.close().await;

    for abort in [false, true] {
        let f = Fixture::new(&server).await;
        let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
        let plan_copy = serde_json::from_value(serde_json::to_value(&f.plan).unwrap()).unwrap();
        drop(ygg::knowledge::migration::Journal::prepare(&f.journal, plan_copy, &config).unwrap());
        let fault = f.journal.join("policy-backup");
        std::fs::write(&fault, "test fault: unavailable policy archive destination").unwrap();
        assert!(!f.migrate(&server, false).await.status.success());
        assert_eq!(f.marker().await, (2, "fenced".into()));
        assert!(config.knowledge_policy_dir.join("runtime.json").exists());
        let corpus_before =
            ygg::knowledge::store::KnowledgeBackup::verify(&f.journal.join("corpus-backup"))
                .unwrap()
                .revision;
        std::fs::remove_file(fault).unwrap();
        success(f.migrate(&server, abort).await);
        assert_eq!(
            f.marker().await,
            (3, if abort { "sql" } else { "okf" }.into())
        );
        assert_eq!(
            ygg::knowledge::store::KnowledgeBackup::verify(&f.journal.join("corpus-backup"))
                .unwrap()
                .revision,
            corpus_before
        );
        if abort {
            assert!(!config.knowledge_policy_dir.join("runtime.json").exists());
            sqlx::query("UPDATE memories SET text='post-abort edit'")
                .execute(&f.pool)
                .await
                .unwrap();
            success(f.migrate(&server, true).await);
            let text: String = sqlx::query_scalar("SELECT text FROM memories")
                .fetch_one(&f.pool)
                .await
                .unwrap();
            assert_eq!(text, "post-abort edit");
        }
        f.pool.close().await;
    }

    let mut f = Fixture::new(&server).await;
    f.plan.transport = "shared".into();
    assert!(!f.migrate(&server, false).await.status.success());
    assert!(!f.journal.exists());
    assert_eq!(f.marker().await, (1, "sql".into()));
    f.pool.close().await;

    let f = Fixture::new(&server).await;
    sqlx::query("ALTER TABLE memories DISABLE TRIGGER ygg_knowledge_write_fence")
        .execute(&f.pool)
        .await
        .unwrap();
    let out = f.migrate(&server, false).await;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("guard"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(f.marker().await, (1, "sql".into()));
    f.pool.close().await;
}

impl Fixture {
    fn rollback_command(&self, server: &Server) -> tokio::process::Command {
        let plan = serde_json::json!({
            "version":1,"transport":"private","source_generation":3,
            "original_export":self.journal.join("stage"),"execution_host":"fixture",
            "all_participating_hosts_listed":true,"hosts":self.plan.hosts,
        });
        let path = self.temp.path().join("rollback-plan.json");
        std::fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
        let mut command = self.command();
        command
            .args(["knowledge", "rollback", "--plan"])
            .arg(path)
            .arg("--journal")
            .arg(self.temp.path().join("recovery"))
            .arg("--pg-bin")
            .arg(&server.bin)
            .arg("--json");
        command
    }
    async fn rollback(&self, server: &Server) -> Output {
        self.rollback_command(server).output().await.unwrap()
    }
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_current_bundle_rollback_preserves_edits_deletions_approvals_and_retries() {
    let server = Server::new();
    let f = Fixture::new(&server).await;
    let old_note: Uuid = sqlx::query_scalar("SELECT memory_id FROM memories")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let old_rule: Uuid = sqlx::query_scalar("SELECT learning_id FROM learnings")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    success(f.migrate(&server, false).await);
    let store = ygg::knowledge::store::KnowledgeStore::open(
        std::path::Path::new(&f.env["YGG_KNOWLEDGE_DIR"]),
        false,
    )
    .unwrap();
    let old = store.find(old_note).unwrap().unwrap();
    store.delete(old.key, &old.revision).unwrap();
    let output = success(
        f.command()
            .args(["remember", "new offline note", "--global", "--json"])
            .env_remove("YGG_AGENT_NAME")
            .env("YGG_DB_MODE", "invalid")
            .output()
            .await
            .unwrap(),
    );
    let note: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = Uuid::parse_str(note["memory_id"].as_str().unwrap()).unwrap();
    let mut current = store.find(id).unwrap().unwrap();
    current.document.body = "edited after cutover\n".into();
    store
        .put(
            &current.document,
            ygg::knowledge::store::ExpectedRevision::Digest(&current.revision),
        )
        .unwrap();
    success(
        f.command()
            .args(["learn", "approve", &old_rule.to_string()])
            .env_remove("YGG_AGENT_NAME")
            .env("YGG_DB_MODE", "invalid")
            .output()
            .await
            .unwrap(),
    );
    let output = success(
        f.command()
            .args(["learn", "propose", "still pending", "--global", "--json"])
            .env_remove("YGG_AGENT_NAME")
            .env("YGG_DB_MODE", "invalid")
            .output()
            .await
            .unwrap(),
    );
    let pending: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let pending_id = Uuid::parse_str(pending["learning_id"].as_str().unwrap()).unwrap();
    let before = store.snapshot();
    assert!(before.diagnostics.is_empty());
    drop(store);
    success(f.rollback(&server).await);
    assert_eq!(f.marker().await, (5, "sql".into()));
    assert!(
        !std::path::Path::new(&f.env["YGG_KNOWLEDGE_POLICY_DIR"])
            .join("runtime.json")
            .exists()
    );
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT memory_id,text FROM memories")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows, vec![(id, "edited after cutover\n".into())]);
    let approved: (String, i32, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT status,applied_count,approved_at FROM learnings WHERE learning_id=$1",
    )
    .bind(old_rule)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(approved.0, "active");
    assert_eq!(approved.1, 7);
    assert!(approved.2.is_some());
    let status: String = sqlx::query_scalar("SELECT status FROM learnings WHERE learning_id=$1")
        .bind(pending_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    sqlx::query("UPDATE memories SET text='new SQL edit after rollback'")
        .execute(&f.pool)
        .await
        .unwrap();
    success(f.rollback(&server).await);
    let text: String = sqlx::query_scalar("SELECT text FROM memories")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "new SQL edit after rollback");
    // An old forward journal cannot reactivate its stale generation.
    assert!(!f.migrate(&server, false).await.status.success());
    sqlx::query("UPDATE knowledge_storage SET generation=generation+1")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(!f.rollback(&server).await.status.success());
    let immutable =
        sqlx::query("UPDATE knowledge_recovery_events SET request_sha256=repeat('0',64)")
            .execute(&f.pool)
            .await
            .unwrap_err();
    assert_eq!(
        immutable.as_database_error().unwrap().code().as_deref(),
        Some("55000")
    );
    sqlx::query("CREATE ROLE recovery_runtime LOGIN")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("GRANT SELECT,INSERT ON knowledge_recovery_events TO recovery_runtime")
        .execute(&f.pool)
        .await
        .unwrap();
    let mut url = url::Url::parse(&f.env["DATABASE_URL"]).unwrap();
    url.set_username("recovery_runtime").unwrap();
    let runtime = PgPoolOptions::new()
        .max_connections(1)
        .connect(url.as_str())
        .await
        .unwrap();
    let denied=sqlx::query("INSERT INTO knowledge_recovery_events(operation_id,step,request_sha256,database_id,corpus_id,generation) VALUES($1,'sql',repeat('0',64),$2,$3,999)").bind(Uuid::new_v4()).bind(f.plan.mappings.database_id).bind(f.plan.mappings.corpus_id).execute(&runtime).await.unwrap_err();
    assert_eq!(
        denied.as_database_error().unwrap().code().as_deref(),
        Some("42501")
    );
    runtime.close().await;
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_unrepresentable_rollback_leaves_okf_selected() {
    let server = Server::new();
    let f = Fixture::new(&server).await;
    success(f.migrate(&server, false).await);
    let store = ygg::knowledge::store::KnowledgeStore::open(
        std::path::Path::new(&f.env["YGG_KNOWLEDGE_DIR"]),
        false,
    )
    .unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT learning_id FROM learnings")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let mut doc = store.find(id).unwrap().unwrap();
    doc.document
        .metadata
        .insert("unknown-recovery-field".into(), "must not lose this".into());
    store
        .put(
            &doc.document,
            ygg::knowledge::store::ExpectedRevision::Digest(&doc.revision),
        )
        .unwrap();
    let out = f.rollback(&server).await;
    assert!(!out.status.success());
    assert_eq!(f.marker().await, (3, "okf".into()));
    let binding: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            std::path::Path::new(&f.env["YGG_KNOWLEDGE_POLICY_DIR"]).join("runtime.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(binding["phase"], "okf");
    success(
        f.command()
            .args([
                "remember",
                "still writable after refused rollback",
                "--global",
            ])
            .env("YGG_DB_MODE", "invalid")
            .output()
            .await
            .unwrap(),
    );
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_killed_rollback_after_sql_commit_resumes_deselection_without_reimport() {
    let server = Server::new();
    let f = Fixture::new(&server).await;
    success(f.migrate(&server, false).await);
    // A fixture-only trigger pauses inside the activation transaction. A queued
    // migration lease then stops the CLI's post-commit lease renewal, providing
    // a deterministic real-process kill boundary without a product fault hook.
    sqlx::raw_sql("CREATE FUNCTION public.fixture_recovery_pause() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.step='sql' THEN PERFORM pg_advisory_xact_lock(610071,1); END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_recovery_pause AFTER INSERT ON public.knowledge_recovery_events FOR EACH ROW EXECUTE FUNCTION public.fixture_recovery_pause();").execute(&f.pool).await.unwrap();
    let gate_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&f.env["DATABASE_URL"])
        .await
        .unwrap();
    clients::register(&mut gate_pool.acquire().await.unwrap())
        .await
        .unwrap();
    let blocker_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&f.env["DATABASE_URL"])
        .await
        .unwrap();
    clients::register(&mut blocker_pool.acquire().await.unwrap())
        .await
        .unwrap();
    let mut gate = gate_pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(610071,1)")
        .execute(&mut *gate)
        .await
        .unwrap();
    let mut blocker = blocker_pool.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let mut child = f
        .rollback_command(&server)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30),async {
        loop {
            assert!(child.try_wait().unwrap().is_none(),"rollback exited before activation checkpoint");
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid=610071 AND objid=1 AND NOT granted)").fetch_one(&f.pool).await.unwrap();
            if waiting {break;}
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let queued = tokio::spawn(async move {
        sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)")
            .execute(&mut *blocker)
            .await
            .unwrap();
        blocker
    });
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND locktype='advisory' AND classid=1497843531 AND objid=1 AND NOT granted)").bind(blocker_pid).fetch_one(&f.pool).await.unwrap();
            if waiting {break;}
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    gate.rollback().await.unwrap();
    let held = tokio::time::timeout(std::time::Duration::from_secs(10), queued)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.marker().await, (5, "sql".into()));
    let runtime = std::path::Path::new(&f.env["YGG_KNOWLEDGE_POLICY_DIR"]).join("runtime.json");
    let binding: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&runtime).unwrap()).unwrap();
    assert_eq!(binding["phase"], "fenced");
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    held.rollback().await.unwrap();
    sqlx::query("UPDATE memories SET text='accepted after SQL commit'")
        .execute(&f.pool)
        .await
        .unwrap();
    success(f.rollback(&server).await);
    assert!(!runtime.exists());
    let text: String = sqlx::query_scalar("SELECT text FROM memories")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "accepted after SQL commit");
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_reverse_receipts")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(receipts, 1);
    gate_pool.close().await;
    blocker_pool.close().await;
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_unrepresentable_sql_schema_refuses_before_fencing() {
    let server = Server::new();
    let f = Fixture::new(&server).await;
    success(f.migrate(&server, false).await);
    sqlx::query("ALTER TABLE memories ADD COLUMN unrepresented TEXT DEFAULT 'must not lose'")
        .execute(&f.pool)
        .await
        .unwrap();
    let out = f.rollback(&server).await;
    assert!(!out.status.success());
    assert_eq!(f.marker().await, (3, "okf".into()));
    let text: String = sqlx::query_scalar("SELECT text FROM memories")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "source note");
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_recovery_events")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(receipts, 0);
    let binding: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            std::path::Path::new(&f.env["YGG_KNOWLEDGE_POLICY_DIR"]).join("runtime.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(binding["phase"], "okf");
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_config_change_after_backup_cannot_fence_or_activate() {
    let server = Server::new();
    let f = Fixture::new(&server).await;
    let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
    let plan_copy = serde_json::from_value(serde_json::to_value(&f.plan).unwrap()).unwrap();
    drop(ygg::knowledge::migration::Journal::prepare(&f.journal, plan_copy, &config).unwrap());
    let backup = ygg::db::deployment_backup::create(
        &config,
        &f.journal.join("source-backup"),
        Some(&server.bin),
        None,
    )
    .await
    .unwrap();
    assert_eq!(backup.version, 2);
    assert!(backup.configuration.is_some());
    let config_dir = std::path::Path::new(&f.env["YGG_CONFIG_DIR"]);
    std::fs::create_dir(config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "# independently edited after backup\n",
    )
    .unwrap();
    let out = f.migrate(&server, false).await;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("configuration changed"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(f.marker().await, (1, "sql".into()));
    assert!(!std::path::Path::new(&f.env["YGG_KNOWLEDGE_DIR"]).exists());
    // Offline integrity verification remains possible after source changes.
    ygg::db::deployment_backup::verify(&f.journal.join("source-backup")).unwrap();
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_generic_document_refuses_rollback_before_selection_changes() {
    let server = Server::new();
    let f = Fixture::new(&server).await;
    success(f.migrate(&server, false).await);
    let path = std::path::Path::new(&f.env["YGG_KNOWLEDGE_DIR"]).join("decision.md");
    let text = "---\ntype: Design Decision\n---\nGeneric knowledge must survive rollback.\n";
    std::fs::write(&path, text).unwrap();
    let output = f.rollback(&server).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SQL cannot represent"));
    assert_eq!(f.marker().await, (3, "okf".into()));
    let binding: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            std::path::Path::new(&f.env["YGG_KNOWLEDGE_POLICY_DIR"]).join("runtime.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(binding["phase"], "okf");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_prefence_abort_drains_sql_and_cancels_delayed_execution() {
    let server = Server::new();
    let f = Fixture::new(&server).await;
    let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
    let plan_copy = serde_json::from_value(serde_json::to_value(&f.plan).unwrap()).unwrap();
    let journal =
        ygg::knowledge::migration::Journal::prepare(&f.journal, plan_copy, &config).unwrap();
    let mut writer = ygg::knowledge::guard::legacy_transaction(&f.pool, true, Some(1))
        .await
        .unwrap();
    sqlx::query("UPDATE memories SET text='in-flight SQL write'")
        .execute(&mut *writer)
        .await
        .unwrap();
    let report = {
        let cancellation = journal.abort(&config);
        tokio::pin!(cancellation);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut cancellation)
                .await
                .is_err()
        );
        let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND classid=1497843531 AND objid=1 AND NOT granted")
            .fetch_one(&mut *writer).await.unwrap();
        assert!(
            waiting > 0,
            "cancellation did not wait for the legacy writer lease"
        );
        writer.commit().await.unwrap();
        cancellation.await.unwrap()
    };
    assert_eq!(report.state, "cancelled");
    assert_eq!(report.generation, 2);
    assert_eq!(f.marker().await, (2, "sql".into()));
    assert!(!f.journal.join("source-backup").exists());
    assert!(!config.knowledge_policy_dir.join("runtime.json").exists());
    drop(journal);
    let text: String = sqlx::query_scalar("SELECT text FROM memories LIMIT 1")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "in-flight SQL write");
    sqlx::query("UPDATE memories SET text='later SQL write'")
        .execute(&f.pool)
        .await
        .unwrap();
    let repeated = success(f.migrate(&server, true).await);
    let repeated: serde_json::Value = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(repeated["state"], "cancelled");
    assert_eq!(repeated["operation"], report.operation.to_string());
    // A fresh process cannot resume the cancelled source generation, even if it
    // relies only on the marker check rather than knowing cancellation receipts.
    assert!(!f.migrate(&server, false).await.status.success());
    assert!(!f.journal.join("source-backup").exists());
    assert_eq!(f.marker().await, (2, "sql".into()));
    let text: String = sqlx::query_scalar("SELECT text FROM memories LIMIT 1")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "later SQL write");
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM knowledge_migration_cancellations")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(receipts, 1);
    assert!(
        sqlx::query("DELETE FROM knowledge_migration_cancellations")
            .execute(&f.pool)
            .await
            .is_err()
    );
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_sql_host_cancellation_requires_receipt_and_prevents_preparation_replay() {
    use ygg::knowledge::{
        fence::{CoordinatorBinding, cancel_sql, prepare_sql, prepare_sql_at_source},
        runtime::{Binding, Phase},
        store::KnowledgeStore,
    };
    let server = Server::new();
    let f = Fixture::new(&server).await;
    let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
    let plan_copy = serde_json::from_value(serde_json::to_value(&f.plan).unwrap()).unwrap();
    drop(ygg::knowledge::migration::Journal::prepare(&f.journal, plan_copy, &config).unwrap());
    let coordinator_bytes = std::fs::read(f.journal.join("migration-intent.json")).unwrap();
    let coordinator_intent: serde_json::Value = serde_json::from_slice(&coordinator_bytes).unwrap();
    let coordinator = CoordinatorBinding {
        migration_operation: serde_json::from_value(coordinator_intent["operation"].clone())
            .unwrap(),
        participant: Uuid::new_v4(),
    };
    let request = ygg::knowledge::document::digest(&coordinator_bytes);
    let registration = ygg::knowledge::fleet::Registration {
        version: 1,
        operation: coordinator.migration_operation,
        request_sha256: request.clone(),
        database_id: f.plan.mappings.database_id,
        source_generation: 1,
        corpus_id: f.plan.mappings.corpus_id,
        participants: vec![coordinator.participant],
    };
    registration.register(&f.pool).await.unwrap();
    let host_root = tempfile::tempdir().unwrap();
    let root = host_root.path().canonicalize().unwrap();
    let corpus = root.join("corpus");
    let policy = root.join("policy");
    KnowledgeStore::open(&corpus, true).unwrap();
    KnowledgeStore::open(&policy, true).unwrap();
    let mut env = f.env.clone();
    env.insert("YGG_KNOWLEDGE_DIR".into(), corpus.display().to_string());
    env.insert(
        "YGG_KNOWLEDGE_POLICY_DIR".into(),
        policy.display().to_string(),
    );
    let (host, _) = ygg::config::database::KnowledgeConfig::load(env).unwrap();
    let binding = Binding {
        version: 1,
        minimum_client: 1,
        generation: 1,
        phase: Phase::Fenced,
        bundle: corpus,
        mappings: serde_json::from_value(serde_json::to_value(&f.plan.mappings).unwrap()).unwrap(),
        agents: BTreeMap::new(),
    };
    let outsider = CoordinatorBinding {
        participant: Uuid::new_v4(),
        ..coordinator
    };
    assert!(
        prepare_sql_at_source(&host, &binding, outsider, &request, &f.pool)
            .await
            .is_err()
    );
    assert!(
        prepare_sql_at_source(&host, &binding, coordinator, &"0".repeat(64), &f.pool)
            .await
            .is_err()
    );
    assert!(!policy.join("runtime.json").exists());
    assert!(!policy.join("sql-fence-1.json").exists());
    prepare_sql_at_source(&host, &binding, coordinator, &request, &f.pool)
        .await
        .unwrap();
    let selection = policy.join("runtime.json");
    let fenced = std::fs::read(&selection).unwrap();
    assert!(
        cancel_sql(&host, &binding, coordinator, &request, &f.pool)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&selection).unwrap(), fenced);
    assert!(!policy.join("sql-fence-1-cancelled.json").exists());
    assert_eq!(registration.cancel(&f.pool).await.unwrap(), 2);
    // A participant without the local cancellation tombstone must still reject
    // a delayed request by consulting the current source and coordinator receipt.
    assert!(
        prepare_sql_at_source(&host, &binding, coordinator, &request, &f.pool)
            .await
            .is_err()
    );
    assert!(!policy.join("sql-fence-1-cancelled.json").exists());
    assert_eq!(std::fs::read(&selection).unwrap(), fenced);
    let other_temp = tempfile::tempdir().unwrap();
    let other_root = other_temp.path().canonicalize().unwrap();
    let other_corpus = other_root.join("corpus");
    let other_policy = other_root.join("policy");
    KnowledgeStore::open(&other_corpus, true).unwrap();
    KnowledgeStore::open(&other_policy, true).unwrap();
    let (other_host, _) = ygg::config::database::KnowledgeConfig::load(BTreeMap::from([
        ("HOME".into(), other_root.display().to_string()),
        (
            "YGG_KNOWLEDGE_DIR".into(),
            other_corpus.display().to_string(),
        ),
        (
            "YGG_KNOWLEDGE_POLICY_DIR".into(),
            other_policy.display().to_string(),
        ),
    ]))
    .unwrap();
    let mut other_binding: Binding =
        serde_json::from_value(serde_json::to_value(&binding).unwrap()).unwrap();
    other_binding.bundle = other_corpus;
    assert!(
        prepare_sql_at_source(&other_host, &other_binding, coordinator, &request, &f.pool)
            .await
            .is_err()
    );
    // Editing the generation to match cannot reuse a cancelled operation UUID.
    other_binding.generation = 2;
    assert!(
        prepare_sql_at_source(&other_host, &other_binding, coordinator, &request, &f.pool)
            .await
            .is_err()
    );
    assert!(!other_policy.join("runtime.json").exists());
    assert!(!other_policy.join("sql-fence-1.json").exists());
    assert!(!other_policy.join("sql-fence-2.json").exists());

    assert!(
        cancel_sql(&host, &binding, coordinator, &"0".repeat(64), &f.pool)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&selection).unwrap(), fenced);
    std::fs::write(&selection, "independent selection").unwrap();
    assert!(
        cancel_sql(&host, &binding, coordinator, &request, &f.pool)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(&selection).unwrap(),
        "independent selection"
    );
    // Original absence is also the state left by a crash before fence publication.
    std::fs::remove_file(&selection).unwrap();
    cancel_sql(&host, &binding, coordinator, &request, &f.pool)
        .await
        .unwrap();
    let tombstone_path = policy.join("sql-fence-1-cancelled.json");
    let tombstone = std::fs::read(&tombstone_path).unwrap();
    assert!(!selection.exists());
    assert!(prepare_sql(&host, &binding, coordinator).is_err());
    // Resume a crash after the tombstone was fsynced but before fence removal.
    std::fs::write(&selection, &fenced).unwrap();
    assert!(prepare_sql(&host, &binding, coordinator).is_err());
    cancel_sql(&host, &binding, coordinator, &request, &f.pool)
        .await
        .unwrap();
    assert!(!selection.exists());
    sqlx::query("UPDATE memories SET text='after host cancellation'")
        .execute(&f.pool)
        .await
        .unwrap();
    cancel_sql(&host, &binding, coordinator, &request, &f.pool)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&tombstone_path).unwrap(), tombstone);
    let text: String = sqlx::query_scalar("SELECT text FROM memories LIMIT 1")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "after host cancellation");
    sqlx::query("UPDATE knowledge_storage SET generation=generation+1 WHERE singleton")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        cancel_sql(&host, &binding, coordinator, &request, &f.pool)
            .await
            .is_err()
    );
    assert!(!selection.exists());
    assert_eq!(std::fs::read(&tombstone_path).unwrap(), tombstone);
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_fleet_registration_serializes_coordinators_and_preserves_cancellation() {
    use ygg::knowledge::fleet::Registration;
    let server = Server::new();
    let f = Fixture::new(&server).await;
    // The coordinator must retain a backup digest in its plan before registering
    // that plan. Registration metadata must not invalidate the source evidence.
    let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
    let backup_path = f.temp.path().canonicalize().unwrap().join("fleet-backup");
    ygg::db::deployment_backup::create(&config, &backup_path, Some(&server.bin), None)
        .await
        .unwrap();
    let backup = ygg::knowledge::source_backup::SourceBackup::open(
        &backup_path,
        f.plan.mappings.database_id,
        1,
    )
    .unwrap();
    let backup_digest = backup.digest().to_owned();
    let first = Registration {
        version: 1,
        operation: Uuid::new_v4(),
        request_sha256: ygg::knowledge::document::digest(b"complete fixture plan"),
        database_id: f.plan.mappings.database_id,
        source_generation: 1,
        corpus_id: f.plan.mappings.corpus_id,
        participants: vec![Uuid::new_v4(), Uuid::new_v4()],
    };
    let second = Registration {
        operation: Uuid::new_v4(),
        ..first.clone()
    };
    let contender = PgPoolOptions::new()
        .max_connections(1)
        .connect(&f.env["DATABASE_URL"])
        .await
        .unwrap();
    let (a, b) = tokio::join!(first.register(&f.pool), second.register(&contender));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let winner = if a.is_ok() { first } else { second };
    assert_eq!(f.marker().await, (1, "sql".into()));
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    backup.verify_on(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(backup.digest(), backup_digest);
    let mut reordered = winner.clone();
    reordered.participants.reverse();
    reordered.register(&contender).await.unwrap();
    for mutation in 0..3 {
        let mut changed = winner.clone();
        match mutation {
            0 => changed.request_sha256 = "0".repeat(64),
            1 => changed.participants.push(Uuid::new_v4()),
            _ => changed.corpus_id = Uuid::new_v4(),
        }
        assert!(changed.register(&f.pool).await.is_err());
        assert!(changed.cancel(&f.pool).await.is_err());
    }
    let mut duplicate = winner.clone();
    duplicate.participants.push(duplicate.participants[0]);
    assert!(duplicate.register(&f.pool).await.is_err());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_fleet_operations")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert!(
        sqlx::query("DELETE FROM knowledge_fleet_operations")
            .execute(&f.pool)
            .await
            .is_err()
    );
    let role = format!("fleet_runtime_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE ROLE {role} LOGIN"))
        .execute(&f.pool)
        .await
        .unwrap();
    let mut runtime_url = url::Url::parse(&f.env["DATABASE_URL"]).unwrap();
    runtime_url.set_username(&role).unwrap();
    let runtime = PgPoolOptions::new()
        .max_connections(1)
        .connect(runtime_url.as_str())
        .await
        .unwrap();
    assert!(
        winner
            .register(&runtime)
            .await
            .unwrap_err()
            .to_string()
            .contains("migration owner")
    );
    assert!(
        winner
            .cancel(&runtime)
            .await
            .unwrap_err()
            .to_string()
            .contains("migration owner")
    );
    runtime.close().await;
    assert_eq!(winner.cancel(&f.pool).await.unwrap(), 2);
    sqlx::query("UPDATE memories SET text='later SQL write'")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(winner.cancel(&contender).await.unwrap(), 2);
    // Genuine legacy content drift still invalidates the same backup.
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        backup
            .verify_on(&mut tx)
            .await
            .unwrap_err()
            .to_string()
            .contains("source memories changed")
    );
    tx.rollback().await.unwrap();
    assert!(winner.register(&f.pool).await.is_err());
    assert_eq!(f.marker().await, (2, "sql".into()));
    let text: String = sqlx::query_scalar("SELECT text FROM memories LIMIT 1")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "later SQL write");
    let next = Registration {
        operation: Uuid::new_v4(),
        source_generation: 2,
        ..winner.clone()
    };
    next.register(&contender).await.unwrap();
    assert_eq!(next.cancel(&contender).await.unwrap(), 3);
    assert!(winner.cancel(&f.pool).await.is_err());
    contender.close().await;
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; starts disposable native PostgreSQL"]
async fn native_backed_participant_retries_only_its_own_policy_additions() {
    use ygg::knowledge::{
        document::digest,
        fence::{CoordinatorBinding, prepare_sql_backed},
        runtime::{Binding, Phase},
        store::KnowledgeStore,
    };
    let server = Server::new();
    let mut f = Fixture::new(&server).await;
    let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
    KnowledgeStore::open(&config.knowledge_dir, true).unwrap();
    let registry =
        ygg::knowledge::identity::IdentityRegistry::open(&config.knowledge_policy_dir, true)
            .unwrap();
    let identity = registry.initialize(true).unwrap();
    f.plan.mappings.corpus_id = identity.corpus_id;
    let backup_path = f
        .temp
        .path()
        .canonicalize()
        .unwrap()
        .join("participant-backup");
    let manifest =
        ygg::db::deployment_backup::create(&config, &backup_path, Some(&server.bin), None)
            .await
            .unwrap();
    let backup_hash = digest(&serde_json::to_vec(&manifest).unwrap());
    let coordinator = CoordinatorBinding {
        migration_operation: Uuid::new_v4(),
        participant: Uuid::new_v4(),
    };
    use ygg::knowledge::fleet::{
        plan::ValidatedPlan,
        protocol::{Action, Request},
    };
    let fleet = ValidatedPlan::parse(&serde_json::json!({
        "version":1,"operation":coordinator.migration_operation,"source_generation":1,
        "mappings":&f.plan.mappings,"agents":[],
        "shared":{"version":1,"remote":"git@example.test:knowledge.git","branch":"main"},
        "expected_remote_commit":"a".repeat(40),
        "source_backup":{"path":&backup_path,"manifest_sha256":&backup_hash},
        "all_participating_hosts_listed":true,"schema_changes_stopped":true,
        "session_preserving_endpoint":true,"remote_writers_stopped":true,
        "participants":[{"id":coordinator.participant,"name":"fixture","protocol":1,
            "endpoint":{"host":"example.test","port":22,"account":"operator","host_key":"ssh-ed25519 AAAA"},
            "corpus":config.knowledge_dir.canonicalize().unwrap(),
            "policy":config.knowledge_policy_dir.canonicalize().unwrap(),"identities":identity,
            "backup":{"path":&backup_path,"manifest_sha256":&backup_hash},
            "knowledge_writers_stopped":true,"external_editors_stopped":true}]
    }).to_string()).unwrap();
    let request_hash = fleet.registration().request_sha256.clone();
    fleet.registration().register(&f.pool).await.unwrap();
    let binding = Binding {
        version: 1,
        minimum_client: 1,
        generation: 1,
        phase: Phase::Fenced,
        bundle: config.knowledge_dir.canonicalize().unwrap(),
        mappings: serde_json::from_value(serde_json::to_value(&f.plan.mappings).unwrap()).unwrap(),
        agents: BTreeMap::new(),
    };
    assert!(
        prepare_sql_backed(
            &config,
            &binding,
            coordinator,
            &request_hash,
            &backup_path,
            &"0".repeat(64),
            &f.pool
        )
        .await
        .is_err()
    );
    assert!(!config.knowledge_policy_dir.join("runtime.json").exists());
    let first = prepare_sql_backed(
        &config,
        &binding,
        coordinator,
        &request_hash,
        &backup_path,
        &backup_hash,
        &f.pool,
    )
    .await
    .unwrap();
    let request = Request::new(&fleet, coordinator.participant, Action::PrepareSql).unwrap();
    let response: serde_json::Value =
        serde_json::from_slice(&success(f.participant(&request.bytes().unwrap()).await).stdout)
            .unwrap();
    let retry: ygg::knowledge::fence::SqlPreparation =
        serde_json::from_value(response["preparation"].clone()).unwrap();
    assert_eq!(first.intent_sha256, retry.intent_sha256);
    let runtime = config.knowledge_policy_dir.join("runtime.json");
    let fenced = std::fs::read(&runtime).unwrap();
    let unrelated = config.knowledge_policy_dir.join("independent.json");
    std::fs::write(&unrelated, b"independent policy").unwrap();
    assert!(
        prepare_sql_backed(
            &config,
            &binding,
            coordinator,
            &request_hash,
            &backup_path,
            &backup_hash,
            &f.pool
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"independent policy");
    assert_eq!(std::fs::read(&runtime).unwrap(), fenced);
    std::fs::remove_file(&unrelated).unwrap();
    std::fs::write(&runtime, b"{}").unwrap();
    assert!(
        prepare_sql_backed(
            &config,
            &binding,
            coordinator,
            &request_hash,
            &backup_path,
            &backup_hash,
            &f.pool
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&runtime).unwrap(), b"{}");
    assert_eq!(f.marker().await, (1, "sql".into()));
    std::fs::write(&runtime, &fenced).unwrap();
    let cancel = Request::new(&fleet, coordinator.participant, Action::CancelSql).unwrap();
    assert!(cancel.execute(&config, &f.pool).await.is_err());
    assert_eq!(std::fs::read(&runtime).unwrap(), fenced);
    fleet.registration().cancel(&f.pool).await.unwrap();
    success(f.participant(&cancel.bytes().unwrap()).await);
    assert!(!runtime.exists());
    assert!(
        config
            .knowledge_policy_dir
            .join("sql-fence-1-cancelled.json")
            .exists()
    );
    sqlx::query("UPDATE memories SET text='after backed cancellation'")
        .execute(&f.pool)
        .await
        .unwrap();
    success(f.participant(&cancel.bytes().unwrap()).await);
    let rejected = f.participant(&request.bytes().unwrap()).await;
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    let invalid = f.participant(b"{}").await;
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
    let text: String = sqlx::query_scalar("SELECT text FROM memories LIMIT 1")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "after backed cancellation");
    f.pool.close().await;
}

struct ParticipantSsh {
    process: std::process::Child,
    endpoint: ygg::knowledge::fleet::transport::Endpoint,
    identity: PathBuf,
    _temp: tempfile::TempDir,
}
impl Drop for ParticipantSsh {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "participant diagnostics: {}",
                std::fs::read_to_string(self._temp.path().join("participant.stderr"))
                    .unwrap_or_default()
            );
            eprintln!(
                "sshd diagnostics: {}",
                std::fs::read_to_string(self._temp.path().join("sshd.log")).unwrap_or_default()
            );
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
impl ParticipantSsh {
    async fn new(f: &Fixture) -> Self {
        let temp = tempfile::Builder::new()
            .prefix(".ygg-fleet-e2e-")
            .tempdir_in(std::env::var_os("HOME").expect("SSH test requires user home"))
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        for name in ["host", "client"] {
            success(
                Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(root.join(name))
                    .output()
                    .unwrap(),
            );
        }
        fn quote(value: &str) -> String {
            format!("'{}'", value.replace('\'', "'\\''"))
        }
        let wrapper = root.join("participant.sh");
        let environment = f
            .env
            .iter()
            .map(|(key, value)| quote(&format!("{key}={value}")))
            .collect::<Vec<_>>()
            .join(" ");
        std::fs::write(&wrapper, format!(
            "#!/bin/sh\nset -eu\ncase \"$SSH_ORIGINAL_COMMAND\" in 'ygg knowledge fleet-participant') ;; *) exit 64 ;; esac\ncd {}\nexec /usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin {} {} knowledge fleet-participant 2>{}\n",
            quote(f.temp.path().to_str().unwrap()), environment, quote(env!("CARGO_BIN_EXE_ygg")), quote(root.join("participant.stderr").to_str().unwrap()))).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let account =
            String::from_utf8(success(Command::new("id").arg("-un").output().unwrap()).stdout)
                .unwrap()
                .trim()
                .to_owned();
        let config = root.join("sshd_config");
        std::fs::write(&config, format!("ListenAddress 127.0.0.1\nPort {port}\nHostKey {0}/host\nPidFile {0}/sshd.pid\nAuthorizedKeysFile {0}/client.pub\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nStrictModes yes\nPermitUserRC no\nForceCommand /bin/sh {0}/participant.sh\n", root.display())).unwrap();
        let log_path = root.join("sshd.log");
        let process = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(config)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log_path).unwrap())
            .spawn()
            .unwrap();
        let host_key = std::fs::read_to_string(root.join("host.pub"))
            .unwrap()
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let mut server = Self {
            process,
            endpoint: ygg::knowledge::fleet::transport::Endpoint {
                host: "127.0.0.1".into(),
                port,
                account,
                host_key,
            },
            identity: root.join("client"),
            _temp: temp,
        };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(
                server.process.try_wait().unwrap().is_none() && std::time::Instant::now() < until,
                "disposable sshd failed: {}",
                std::fs::read_to_string(&log_path).unwrap()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        server
    }
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and OpenSSH server/client tools; starts disposable PG and SSH"]
async fn native_authenticated_ssh_participant_preparation_and_cancellation() {
    use ygg::knowledge::{
        document::digest,
        fleet::{
            journal::Journal,
            plan::ValidatedPlan,
            protocol::{self, Action},
        },
        store::KnowledgeStore,
    };
    let server = Server::new();
    let mut f = Fixture::new(&server).await;
    let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
    KnowledgeStore::open(&config.knowledge_dir, true).unwrap();
    let identities =
        ygg::knowledge::identity::IdentityRegistry::open(&config.knowledge_policy_dir, true)
            .unwrap()
            .initialize(true)
            .unwrap();
    f.plan.mappings.corpus_id = identities.corpus_id;
    let backup = f.temp.path().canonicalize().unwrap().join("host-backup");
    let manifest = ygg::db::deployment_backup::create(&config, &backup, Some(&server.bin), None)
        .await
        .unwrap();
    let hash = digest(&serde_json::to_vec(&manifest).unwrap());
    let ssh = ParticipantSsh::new(&f).await;
    let participant = Uuid::new_v4();
    let plan = ValidatedPlan::parse(&serde_json::json!({
        "version":1,"operation":Uuid::new_v4(),"source_generation":1,"mappings":&f.plan.mappings,"agents":[],
        "shared":{"version":1,"remote":"git@example.test:knowledge.git","branch":"main"},
        "expected_remote_commit":"a".repeat(40),"source_backup":{"path":backup,"manifest_sha256":hash},
        "all_participating_hosts_listed":true,"schema_changes_stopped":true,"session_preserving_endpoint":true,"remote_writers_stopped":true,
        "participants":[{"id":participant,"name":"ssh-host","protocol":1,"endpoint":&ssh.endpoint,
            "corpus":config.knowledge_dir.canonicalize().unwrap(),"policy":config.knowledge_policy_dir.canonicalize().unwrap(),
            "identities":identities,"backup":{"path":backup,"manifest_sha256":hash},
            "knowledge_writers_stopped":true,"external_editors_stopped":true}]
    }).to_string()).unwrap();
    let journal = Journal::prepare(&f.journal, plan).unwrap();
    journal
        .plan()
        .unwrap()
        .registration()
        .register(&f.pool)
        .await
        .unwrap();
    let runtime = config.knowledge_policy_dir.join("runtime.json");
    // Wrong client key must fail before any participant authority mutation.
    assert!(
        protocol::call(
            &journal,
            participant,
            Action::PrepareSql,
            Some(&ssh.identity.with_file_name("host"))
        )
        .await
        .is_err()
    );
    assert!(!runtime.exists());
    let prepared = protocol::call(
        &journal,
        participant,
        Action::PrepareSql,
        Some(&ssh.identity),
    )
    .await
    .unwrap();
    assert!(runtime.exists());
    let repeated = protocol::call(
        &journal,
        participant,
        Action::PrepareSql,
        Some(&ssh.identity),
    )
    .await
    .unwrap();
    assert_eq!(
        prepared.preparation().intent_sha256,
        repeated.preparation().intent_sha256
    );
    let prepared_digest = journal
        .prepare_hosts(&config, &f.pool, Some(&ssh.identity))
        .await
        .unwrap();
    assert_eq!(
        journal
            .prepare_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap(),
        prepared_digest
    );
    assert!(f.journal.join("fleet-prepared.json").exists());
    assert!(
        protocol::call(
            &journal,
            participant,
            Action::CancelSql,
            Some(&ssh.identity)
        )
        .await
        .is_err()
    );
    assert!(runtime.exists());
    journal
        .plan()
        .unwrap()
        .registration()
        .cancel(&f.pool)
        .await
        .unwrap();
    let cancelled = protocol::call(
        &journal,
        participant,
        Action::CancelSql,
        Some(&ssh.identity),
    )
    .await
    .unwrap();
    assert_eq!(cancelled.action(), Action::CancelSql);
    assert!(!runtime.exists());
    sqlx::query("UPDATE memories SET text='after SSH cancellation'")
        .execute(&f.pool)
        .await
        .unwrap();
    protocol::call(
        &journal,
        participant,
        Action::CancelSql,
        Some(&ssh.identity),
    )
    .await
    .unwrap();
    assert!(
        protocol::call(
            &journal,
            participant,
            Action::PrepareSql,
            Some(&ssh.identity)
        )
        .await
        .is_err()
    );
    let text: String = sqlx::query_scalar("SELECT text FROM memories LIMIT 1")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "after SSH cancellation");
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and OpenSSH tools; starts disposable PG and two SSH servers"]
async fn native_partial_fleet_cancellation_includes_unprepared_host() {
    use ygg::knowledge::{
        document::digest,
        fleet::{journal::Journal, plan::ValidatedPlan},
        store::KnowledgeStore,
    };
    let server = Server::new();
    let mut first = Fixture::new(&server).await;
    let config = ygg::config::database::DeploymentConfig::load(first.env.clone()).unwrap();
    KnowledgeStore::open(&config.knowledge_dir, true).unwrap();
    let identities =
        ygg::knowledge::identity::IdentityRegistry::open(&config.knowledge_policy_dir, true)
            .unwrap()
            .initialize(true)
            .unwrap();
    first.plan.mappings.corpus_id = identities.corpus_id;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut env = first.env.clone();
    for (key, name) in [
        ("YGG_CONFIG_DIR", "config"),
        ("YGG_DATA_DIR", "data"),
        ("YGG_KNOWLEDGE_DIR", "corpus"),
        ("YGG_KNOWLEDGE_POLICY_DIR", "policy"),
    ] {
        env.insert(key.into(), root.join(name).display().to_string());
    }
    let second = Fixture {
        pool: first.pool.clone(),
        temp,
        env,
        plan: serde_json::from_value(serde_json::to_value(&first.plan).unwrap()).unwrap(),
        journal: root.join("journal"),
    };
    let config2 = ygg::config::database::DeploymentConfig::load(second.env.clone()).unwrap();
    KnowledgeStore::open(&config2.knowledge_dir, true).unwrap();
    KnowledgeStore::open(&config2.knowledge_policy_dir, true).unwrap();
    std::fs::write(
        config2.knowledge_policy_dir.join("identity.json"),
        serde_json::to_vec(&identities).unwrap(),
    )
    .unwrap();
    let mut backups = Vec::new();
    for (fixture, config) in [(&first, &config), (&second, &config2)] {
        let path = fixture.temp.path().canonicalize().unwrap().join("backup");
        let manifest = ygg::db::deployment_backup::create(config, &path, Some(&server.bin), None)
            .await
            .unwrap();
        backups.push(serde_json::json!({"path":path,"manifest_sha256":digest(&serde_json::to_vec(&manifest).unwrap())}));
    }
    let ssh1 = ParticipantSsh::new(&first).await;
    let ssh2 = ParticipantSsh::new(&second).await;
    let participants = [Uuid::new_v4(), Uuid::new_v4()];
    let hosts = [(&config, &ssh1), (&config2, &ssh2)].into_iter().enumerate().map(|(i,(config,ssh))| serde_json::json!({
        "id":participants[i],"name":format!("host-{i}"),"protocol":1,"endpoint":&ssh.endpoint,
        "corpus":config.knowledge_dir.canonicalize().unwrap(),"policy":config.knowledge_policy_dir.canonicalize().unwrap(),
        "identities":&identities,"backup":&backups[i],"knowledge_writers_stopped":true,"external_editors_stopped":true
    })).collect::<Vec<_>>();
    let plan = ValidatedPlan::parse(&serde_json::json!({
        "version":1,"operation":Uuid::new_v4(),"source_generation":1,"mappings":&first.plan.mappings,"agents":[],
        "shared":{"version":1,"remote":"git@example.test:knowledge.git","branch":"main"},"expected_remote_commit":"a".repeat(40),
        "source_backup":backups[0],"all_participating_hosts_listed":true,"schema_changes_stopped":true,
        "session_preserving_endpoint":true,"remote_writers_stopped":true,"participants":hosts
    }).to_string()).unwrap();
    let hash = plan.registration().request_sha256.clone();
    let journal = Journal::prepare(&first.journal, plan).unwrap();
    // The supplied key authenticates host 1 but not host 2.
    assert!(
        journal
            .prepare_hosts(&config, &first.pool, Some(&ssh1.identity))
            .await
            .is_err()
    );
    assert!(config.knowledge_policy_dir.join("runtime.json").exists());
    assert!(!config2.knowledge_policy_dir.join("runtime.json").exists());
    assert!(
        first
            .journal
            .join(format!("prepared-{}.json", participants[0]))
            .exists()
    );
    assert!(!first.journal.join("fleet-prepared.json").exists());
    // Cancel with host 2's key: host 1 is unreachable, but host 2 must still
    // receive its cancellation tombstone even though it never prepared.
    assert!(
        journal
            .cancel_hosts(&first.pool, Some(&ssh2.identity))
            .await
            .is_err()
    );
    assert!(config.knowledge_policy_dir.join("runtime.json").exists());
    assert!(
        config2
            .knowledge_policy_dir
            .join("sql-fence-1-cancelled.json")
            .exists()
    );
    assert!(
        first
            .journal
            .join(format!("cancelled-{}.json", participants[1]))
            .exists()
    );
    assert!(
        !first
            .journal
            .join(format!("cancelled-{}.json", participants[0]))
            .exists()
    );
    assert!(!first.journal.join("fleet-cancelled.json").exists());
    sqlx::query("UPDATE memories SET text='after partial fleet cancellation'")
        .execute(&first.pool)
        .await
        .unwrap();
    drop(journal);
    // Repair only this disposable server's authorized client key, then resume.
    std::fs::copy(
        ssh2.identity.with_extension("pub"),
        ssh1.identity.with_extension("pub"),
    )
    .unwrap();
    let journal = Journal::resume(&first.journal, &hash).unwrap();
    let seal = journal
        .cancel_hosts(&first.pool, Some(&ssh2.identity))
        .await
        .unwrap();
    assert_eq!(
        journal
            .cancel_hosts(&first.pool, Some(&ssh2.identity))
            .await
            .unwrap(),
        seal
    );
    assert!(first.journal.join("fleet-cancelled.json").exists());
    assert!(!config.knowledge_policy_dir.join("runtime.json").exists());
    assert!(
        config2
            .knowledge_policy_dir
            .join("sql-fence-1-cancelled.json")
            .exists()
    );
    assert!(!config2.knowledge_policy_dir.join("runtime.json").exists());
    assert!(
        journal
            .prepare_hosts(&config, &first.pool, Some(&ssh1.identity))
            .await
            .is_err()
    );
    let text: String = sqlx::query_scalar("SELECT text FROM memories LIMIT 1")
        .fetch_one(&first.pool)
        .await
        .unwrap();
    assert_eq!(text, "after partial fleet cancellation");
    first.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and OpenSSH; disposable fleet fence/abort qualification"]
async fn native_fleet_owned_fence_resume_and_abort() {
    fleet_fence_fixture(None).await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and OpenSSH; committed fleet host finalization"]
async fn native_fleet_finalization_resumes_directory_states_and_preserves_writes() {
    for completed_renames in 0..=2 {
        fleet_fence_fixture(Some(completed_renames)).await;
    }
}

async fn fleet_fence_fixture(finalize_steps: Option<usize>) {
    use ygg::knowledge::{
        document::digest,
        fleet::{
            journal::Journal,
            plan::ValidatedPlan,
            protocol::{self, Action},
        },
        store::KnowledgeStore,
    };
    let server = Server::new();
    let mut f = Fixture::new(&server).await;
    sqlx::query("INSERT INTO memories(text,user_id) SELECT 'bulk note ' || n,'alice' FROM generate_series(1,40) n")
        .execute(&f.pool).await.unwrap();
    let config = ygg::config::database::DeploymentConfig::load(f.env.clone()).unwrap();
    KnowledgeStore::open(&config.knowledge_dir, true).unwrap();
    let identities =
        ygg::knowledge::identity::IdentityRegistry::open(&config.knowledge_policy_dir, true)
            .unwrap()
            .initialize(true)
            .unwrap();
    f.plan.mappings.corpus_id = identities.corpus_id;
    let backup = f.temp.path().canonicalize().unwrap().join("host-backup");
    let manifest = ygg::db::deployment_backup::create(&config, &backup, Some(&server.bin), None)
        .await
        .unwrap();
    let hash = digest(&serde_json::to_vec(&manifest).unwrap());
    let ssh = ParticipantSsh::new(&f).await;
    let participant = Uuid::new_v4();
    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    let seed = f.temp.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "knowledge"]);
    std::fs::write(seed.join("README.txt"), "retained remote metadata\n").unwrap();
    git(&seed, &["add", "README.txt"]);
    git(&seed, &["commit", "-m", "initial"]);
    let remote = f.temp.path().join("remote.git");
    git(f.temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
    git(
        &seed,
        &[
            "push",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/knowledge",
        ],
    );
    let base = git(&seed, &["rev-parse", "HEAD"]);
    let plan = ValidatedPlan::parse(&serde_json::json!({
        "version":1,"operation":Uuid::new_v4(),"source_generation":1,"mappings":&f.plan.mappings,"agents":[],
        "shared":{"version":1,"remote":remote,"branch":"knowledge"},
        "expected_remote_commit":base,"source_backup":{"path":backup,"manifest_sha256":hash},
        "all_participating_hosts_listed":true,"schema_changes_stopped":true,"session_preserving_endpoint":true,"remote_writers_stopped":true,
        "participants":[{"id":participant,"name":"ssh-host","protocol":1,"endpoint":&ssh.endpoint,
            "corpus":config.knowledge_dir.canonicalize().unwrap(),"policy":config.knowledge_policy_dir.canonicalize().unwrap(),
            "identities":identities,"backup":{"path":backup,"manifest_sha256":hash},
            "knowledge_writers_stopped":true,"external_editors_stopped":true}]
    }).to_string()).unwrap();
    let request_hash = plan.registration().request_sha256.clone();
    let journal = Journal::prepare(&f.journal, plan).unwrap();
    let runtime = config.knowledge_policy_dir.join("runtime.json");
    journal
        .prepare_hosts(&config, &f.pool, Some(&ssh.identity))
        .await
        .unwrap();
    assert!(
        journal
            .abort_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert!(
        protocol::call(
            &journal,
            participant,
            Action::InspectSql,
            Some(&ssh.identity)
        )
        .await
        .is_err()
    );
    assert!(
        protocol::call(&journal, participant, Action::AbortSql, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert!(runtime.exists());
    use sqlx::Connection;
    let mut writer_connection = sqlx::PgConnection::connect_with(&f.pool.connect_options())
        .await
        .unwrap();
    let mut observer = sqlx::PgConnection::connect_with(&f.pool.connect_options())
        .await
        .unwrap();
    ygg::knowledge::clients::register(&mut writer_connection)
        .await
        .unwrap();
    ygg::knowledge::clients::register(&mut observer)
        .await
        .unwrap();
    let mut writer = writer_connection.begin().await.unwrap();
    sqlx::query("UPDATE memories SET text=text")
        .execute(&mut *writer)
        .await
        .unwrap();
    {
        let pending = journal.fence_hosts(&config, &f.pool, Some(&ssh.identity));
        tokio::pin!(pending);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            tokio::select! {
                result = &mut pending => panic!("fence finished before SQL writer drained: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid=1497843531 AND objid=1 AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))")
                .fetch_one(&mut observer).await.unwrap();
            if waiting {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "coordinator did not reach the SQL generation lease"
            );
        }
        writer.commit().await.unwrap();
        assert_eq!(pending.await.unwrap(), 2);
    }
    assert!(
        sqlx::query("UPDATE memories SET text='must be fenced'")
            .execute(&f.pool)
            .await
            .is_err()
    );
    assert!(
        journal
            .cancel_hosts(&f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert!(
        protocol::call(
            &journal,
            participant,
            Action::PrepareSql,
            Some(&ssh.identity)
        )
        .await
        .is_err()
    );
    drop(journal);
    let journal = Journal::resume(&f.journal, &request_hash).unwrap();
    assert_eq!(
        journal
            .fence_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap(),
        2
    );
    let exported = journal
        .stage_hosts(&config, &f.pool, Some(&ssh.identity))
        .await
        .unwrap();
    assert_eq!(exported.generation, 2);
    assert_eq!(
        exported.entries.len(),
        42,
        "fleet publication must exceed the ordinary 32-change limit"
    );
    assert!(f.journal.join("fleet-export.json").exists());
    assert_eq!(
        journal
            .stage_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap(),
        exported
    );
    let staged_document = f
        .journal
        .join("stage")
        .join(exported.entries[0].key.relative_path());
    let original_document = std::fs::read(&staged_document).unwrap();
    std::fs::write(&staged_document, b"independent stage edit").unwrap();
    assert!(
        journal
            .stage_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(&staged_document).unwrap(),
        b"independent stage edit"
    );
    std::fs::write(&staged_document, original_document).unwrap();
    let published = journal
        .publish_hosts(&config, &f.pool, Some(&ssh.identity))
        .await
        .unwrap();
    assert_eq!(git(&remote, &["rev-parse", "knowledge"]), published.commit);
    assert_eq!(
        git(
            &remote,
            &["show", &format!("{}:README.txt", published.commit)]
        ),
        "retained remote metadata"
    );
    assert_eq!(
        journal
            .publish_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap(),
        published
    );
    assert_eq!(git(&remote, &["rev-list", "--count", "knowledge"]), "2");
    assert!(
        runtime.exists(),
        "Git publication must leave the host fenced"
    );
    // Model a lost local completion write after push and transport confirmation.
    std::fs::remove_file(f.journal.join("fleet-published.json")).unwrap();
    drop(journal);
    let journal = Journal::resume(&f.journal, &request_hash).unwrap();
    assert_eq!(
        journal
            .publish_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap(),
        published
    );
    assert_eq!(git(&remote, &["rev-list", "--count", "knowledge"]), "2");
    use std::os::unix::fs::MetadataExt;
    let original_corpus = std::fs::metadata(&config.knowledge_dir).unwrap();
    let ready_seal = journal
        .ready_hosts(&config, &f.pool, Some(&ssh.identity))
        .await
        .unwrap();
    assert_eq!(
        journal
            .ready_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap(),
        ready_seal
    );
    let ready_response =
        protocol::call_ready(&journal, participant, &published, Some(&ssh.identity))
            .await
            .unwrap();
    let ready = ready_response.readiness().unwrap();
    let candidate = ready.staging.join("candidate");
    let candidate_backup = ready.staging.join("candidate-backup");
    let swap_path = ready.staging.join("directory-swap.json");
    let swap_bytes = std::fs::read(&swap_path).unwrap();
    assert_eq!(
        ygg::knowledge::document::digest(&swap_bytes),
        ready.swap_sha256
    );
    let swap: ygg::knowledge::store::DirectorySwapPlan =
        serde_json::from_slice(&swap_bytes).unwrap();
    assert_eq!(
        swap.inspect().unwrap(),
        ygg::knowledge::store::DirectorySwapState::Prepared
    );
    std::fs::write(&swap_path, "independent edit").unwrap();
    assert!(
        protocol::call_ready(&journal, participant, &published, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(&swap_path).unwrap(),
        "independent edit"
    );
    std::fs::remove_file(&swap_path).unwrap();
    assert!(
        protocol::call_ready(&journal, participant, &published, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert!(
        !swap_path.exists(),
        "retry must not recreate missing sealed evidence"
    );
    std::fs::write(&swap_path, &swap_bytes).unwrap();
    // Exercise the database activation receipt against actual authenticated
    // readiness. Roll back each transaction: this fixture continues through
    // pre-activation abort and must not expose an active host prematurely.
    let activation_json = std::fs::read_to_string(f.journal.join("fleet-ready.json")).unwrap();
    let prepared_sha = ygg::knowledge::document::digest(
        &std::fs::read(f.journal.join("fleet-prepared.json")).unwrap(),
    );
    let source_sha = &journal.plan().unwrap().plan().source_backup.manifest_sha256;
    let operation = journal.plan().unwrap().plan().operation;
    for case in 0..9 {
        let mut payload: serde_json::Value = serde_json::from_str(&activation_json).unwrap();
        match case {
            3 => payload["participants"] = serde_json::json!([]),
            4 => {
                payload["participants"][0]["readiness"]["publication"]["commit"] =
                    serde_json::json!("f".repeat(40))
            }
            5 => {
                payload["participants"][0]["readiness"]["swap_sha256"] =
                    serde_json::json!("invalid")
            }
            7 => {
                let duplicate = payload["participants"][0].clone();
                payload["participants"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
            }
            _ => {}
        }
        let bytes = serde_json::to_string(&payload).unwrap();
        let ready_sha = if case == 1 {
            "0".repeat(64)
        } else {
            ygg::knowledge::document::digest(bytes.as_bytes())
        };
        let evidence_sha = if case == 2 {
            "0".repeat(64)
        } else {
            prepared_sha.clone()
        };
        let mut tx = f.pool.begin().await.unwrap();
        if case != 0 {
            sqlx::query("UPDATE knowledge_storage SET backend='okf',generation=generation+1 WHERE singleton").execute(&mut *tx).await.unwrap();
        }
        let result = sqlx::query("INSERT INTO knowledge_fleet_activations(operation_id,generation,prepared_sha256,backup_sha256,ready_sha256,ready_json) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(operation).bind(if case == 6 { 4_i64 } else { 3_i64 }).bind(&evidence_sha).bind(source_sha).bind(&ready_sha).bind(&bytes)
            .execute(&mut *tx).await;
        if case == 8 {
            result.unwrap();
            let saved: String = sqlx::query_scalar(
                "SELECT ready_sha256 FROM knowledge_fleet_activations WHERE operation_id=$1",
            )
            .bind(operation)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            assert_eq!(saved, ready_sha);
            sqlx::query("SAVEPOINT runtime_activation")
                .execute(&mut *tx)
                .await
                .unwrap();
            let role = format!("activation_runtime_{}", Uuid::new_v4().simple());
            sqlx::query(&format!("CREATE ROLE {role}"))
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query(&format!(
                "GRANT INSERT ON knowledge_fleet_activations TO {role}"
            ))
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query(&format!("SET LOCAL ROLE {role}"))
                .execute(&mut *tx)
                .await
                .unwrap();
            let denied = sqlx::query("INSERT INTO knowledge_fleet_activations(operation_id,generation,prepared_sha256,backup_sha256,ready_sha256,ready_json) VALUES($1,3,$2,$3,$4,$5)")
                .bind(operation).bind(&prepared_sha).bind(source_sha).bind(&ready_sha).bind(&bytes).execute(&mut *tx).await.unwrap_err();
            assert_eq!(
                denied
                    .as_database_error()
                    .and_then(|error| error.code())
                    .as_deref(),
                Some("42501")
            );
            sqlx::query("ROLLBACK TO SAVEPOINT runtime_activation")
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("RELEASE SAVEPOINT runtime_activation")
                .execute(&mut *tx)
                .await
                .unwrap();
            for mutation in [
                "UPDATE knowledge_fleet_activations SET ready_sha256=ready_sha256",
                "DELETE FROM knowledge_fleet_activations",
                "TRUNCATE knowledge_fleet_activations",
            ] {
                sqlx::query("SAVEPOINT immutable_activation")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                assert!(sqlx::query(mutation).execute(&mut *tx).await.is_err());
                sqlx::query("ROLLBACK TO SAVEPOINT immutable_activation")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                sqlx::query("RELEASE SAVEPOINT immutable_activation")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
        } else {
            assert!(result.is_err(), "activation mutation {case} must fail");
        }
        tx.rollback().await.unwrap();
    }
    let activation_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM knowledge_fleet_activations")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(activation_count, 0);
    let marker: (String, i64) =
        sqlx::query_as("SELECT backend,generation FROM knowledge_storage WHERE singleton")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(marker, ("fenced".into(), 2));

    assert_eq!(
        ygg::knowledge::store::KnowledgeBackup::verify_restored(&candidate_backup, &candidate)
            .unwrap()
            .revision,
        ready.archive_revision
    );
    let after_ready = std::fs::metadata(&config.knowledge_dir).unwrap();
    assert_eq!(
        (after_ready.dev(), after_ready.ino()),
        (original_corpus.dev(), original_corpus.ino())
    );
    assert!(
        runtime.exists(),
        "readiness must leave the original host fenced"
    );
    assert!(f.journal.join("fleet-ready.json").exists());
    if let Some(completed_renames) = finalize_steps {
        use ygg::knowledge::fence::finalize_sql_backed;
        assert!(
            finalize_sql_backed(
                &config,
                journal.plan().unwrap(),
                participant,
                &ready_seal,
                &f.pool
            )
            .await
            .is_err()
        );
        assert_eq!(
            journal
                .ready_hosts(&config, &f.pool, Some(&ssh.identity))
                .await
                .unwrap(),
            ready_seal
        );
        let activated = journal
            .activate_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap();
        assert_eq!(activated.generation, 3);
        assert_eq!(activated.readiness_sha256, ready_seal);
        assert_eq!(activated.publication, published);
        // Lost local completion is reconciled from immutable SQL authority.
        std::fs::remove_file(f.journal.join("fleet-activated.json")).unwrap();
        assert_eq!(
            journal
                .activate_hosts(&config, &f.pool, Some(&ssh.identity))
                .await
                .unwrap(),
            activated
        );
        assert!(
            finalize_sql_backed(
                &config,
                journal.plan().unwrap(),
                participant,
                &"0".repeat(64),
                &f.pool
            )
            .await
            .is_err()
        );
        for _ in 0..completed_renames {
            swap.advance().unwrap();
        }
        if completed_renames == 1 {
            assert!(!config.knowledge_dir.exists());
        }
        if completed_renames == 2 {
            // Recover transport publication before the final binding write.
            std::fs::write(
                config.knowledge_policy_dir.join("shared.json"),
                serde_json::to_vec(&journal.plan().unwrap().plan().shared).unwrap(),
            )
            .unwrap();
        }
        if completed_renames == 0 {
            std::fs::write(
                candidate.join("independent.txt"),
                "preserve after activation",
            )
            .unwrap();
            assert!(
                finalize_sql_backed(
                    &config,
                    journal.plan().unwrap(),
                    participant,
                    &ready_seal,
                    &f.pool
                )
                .await
                .is_err()
            );
            assert_eq!(
                std::fs::read_to_string(candidate.join("independent.txt")).unwrap(),
                "preserve after activation"
            );
            assert_eq!(
                swap.inspect().unwrap(),
                ygg::knowledge::store::DirectorySwapState::Prepared
            );
            std::fs::remove_file(candidate.join("independent.txt")).unwrap();
        }
        let remote_finalized = protocol::call_finalize(
            &journal,
            participant,
            &published,
            &ready_seal,
            Some(&ssh.identity),
        )
        .await
        .unwrap();
        assert_eq!(remote_finalized.readiness(), Some(ready));
        let selected = finalize_sql_backed(
            &config,
            journal.plan().unwrap(),
            participant,
            &ready_seal,
            &f.pool,
        )
        .await
        .unwrap();
        assert_eq!(selected.readiness, *ready);
        assert_eq!(
            remote_finalized.selection_sha256(),
            Some(selected.selected_sha256.as_str())
        );
        assert_eq!(
            journal
                .finalize_hosts(&config, &f.pool, Some(&ssh.identity))
                .await
                .unwrap(),
            activated
        );
        assert!(f.journal.join("fleet-finalized.json").exists());
        let binding: ygg::knowledge::runtime::Binding =
            serde_json::from_slice(&std::fs::read(&runtime).unwrap()).unwrap();
        assert!(binding.phase == ygg::knowledge::runtime::Phase::Okf);
        assert_eq!(binding.generation, 3);
        assert_eq!(
            swap.inspect().unwrap(),
            ygg::knowledge::store::DirectorySwapState::CandidateInstalled
        );
        let remembered = f
            .command()
            .args(["remember", "after fleet finalization", "--global", "--json"])
            .output()
            .await
            .unwrap();
        assert!(
            remembered.status.success(),
            "{}",
            String::from_utf8_lossy(&remembered.stderr)
        );
        assert_eq!(
            finalize_sql_backed(
                &config,
                journal.plan().unwrap(),
                participant,
                &ready_seal,
                &f.pool
            )
            .await
            .unwrap(),
            selected
        );
        assert_eq!(
            journal
                .finalize_hosts(&config, &f.pool, Some(&ssh.identity))
                .await
                .unwrap(),
            activated
        );
        let shared = ygg::knowledge::shared::SharedGit::open(
            &config.knowledge_dir,
            journal.plan().unwrap().plan().shared.clone(),
        )
        .unwrap();
        let snapshot = shared.refresh().unwrap();
        assert!(
            snapshot
                .files
                .values()
                .any(|bytes| String::from_utf8_lossy(bytes).contains("after fleet finalization"))
        );
        assert_ne!(snapshot.commit, published.commit);
        assert!(
            ygg::knowledge::store::KnowledgeBackup::verify_restored(
                &candidate_backup,
                &config.knowledge_dir
            )
            .is_err(),
            "new writes must survive old readiness evidence"
        );
        assert!(
            journal
                .abort_hosts(&config, &f.pool, Some(&ssh.identity))
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE memories SET text='forbidden'")
                .execute(&f.pool)
                .await
                .is_err()
        );
        let baseline: i32 = sqlx::query_scalar(
            "SELECT imported_count FROM knowledge_usage WHERE corpus_id=$1 LIMIT 1",
        )
        .bind(f.plan.mappings.corpus_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(baseline, 7);
        f.pool.close().await;
        return;
    }
    std::fs::write(candidate.join("independent.txt"), "do not overwrite").unwrap();
    assert!(
        protocol::call_ready(&journal, participant, &published, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(candidate.join("independent.txt")).unwrap(),
        "do not overwrite"
    );
    std::fs::remove_file(candidate.join("independent.txt")).unwrap();
    let mut wrong = serde_json::to_value(&published).unwrap();
    wrong["manifest_sha256"] = serde_json::json!("e".repeat(64));
    let wrong = serde_json::from_value(wrong).unwrap();
    assert!(
        protocol::call_ready(&journal, participant, &wrong, Some(&ssh.identity))
            .await
            .is_err()
    );
    ygg::knowledge::store::KnowledgeBackup::verify_restored(&candidate_backup, &candidate).unwrap();
    git(&seed, &["fetch", remote.to_str().unwrap(), "knowledge"]);
    git(&seed, &["reset", "--hard", "FETCH_HEAD"]);
    std::fs::write(seed.join("independent.txt"), "external change\n").unwrap();
    git(&seed, &["add", "independent.txt"]);
    git(&seed, &["commit", "-m", "independent"]);
    git(
        &seed,
        &[
            "push",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/knowledge",
        ],
    );
    let independent = git(&remote, &["rev-parse", "knowledge"]);
    assert!(
        journal
            .publish_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert!(
        journal
            .abort_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert_eq!(git(&remote, &["rev-parse", "knowledge"]), independent);
    // Fixture-only operator reconciliation restores the exact publication. The
    // coordinator itself must never reset a remote branch to accomplish abort.
    git(
        &remote,
        &["update-ref", "refs/heads/knowledge", &published.commit],
    );
    let original_runtime = std::fs::read(&runtime).unwrap();
    std::fs::remove_file(&runtime).unwrap();
    assert!(
        journal
            .fence_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert!(
        !runtime.exists(),
        "inspection must not republish an absent host fence"
    );
    std::fs::write(&runtime, &original_runtime).unwrap();
    let seal_path = f.journal.join("fleet-prepared.json");
    let seal = std::fs::read(&seal_path).unwrap();
    std::fs::write(&seal_path, "[]").unwrap();
    assert!(
        journal
            .abort_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert!(runtime.exists());
    std::fs::write(&seal_path, seal).unwrap();
    // Commit SQL abort but fail host authentication. Resume must restore this
    // host without replaying the source dump over writes made after SQL reopened.
    assert!(
        journal
            .abort_hosts(&config, &f.pool, Some(&ssh.identity.with_file_name("host")))
            .await
            .is_err()
    );
    assert!(runtime.exists());
    assert!(!f.journal.join("fleet-aborted.json").exists());
    let state: (String, i64) =
        sqlx::query_as("SELECT backend,generation FROM knowledge_storage WHERE singleton")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(state, ("sql".into(), 3));
    sqlx::query("UPDATE memories SET text='after fleet abort'")
        .execute(&f.pool)
        .await
        .unwrap();
    drop(journal);
    let offline = f.temp.path().join("offline-remote.git");
    std::fs::rename(&remote, &offline).unwrap();
    let journal = Journal::resume(&f.journal, &request_hash).unwrap();
    let aborted = journal
        .abort_hosts(&config, &f.pool, Some(&ssh.identity))
        .await
        .unwrap();
    assert!(!runtime.exists());
    assert_eq!(
        journal
            .abort_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap(),
        aborted
    );
    assert!(
        journal
            .fence_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert!(
        protocol::call(
            &journal,
            participant,
            Action::PrepareSql,
            Some(&ssh.identity)
        )
        .await
        .is_err()
    );
    assert!(
        journal
            .stage_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .is_err()
    );
    assert_eq!(
        ygg::knowledge::export::verify(&f.journal.join("stage")).unwrap(),
        exported
    );
    // SQL return is already committed: host reconciliation must not depend on
    // remote availability or overwrite independent later Git changes.
    std::fs::rename(&offline, &remote).unwrap();
    git(
        &remote,
        &["update-ref", "refs/heads/knowledge", &independent],
    );
    assert_eq!(
        journal
            .abort_hosts(&config, &f.pool, Some(&ssh.identity))
            .await
            .unwrap(),
        aborted
    );
    assert_eq!(git(&remote, &["rev-parse", "knowledge"]), independent);
    assert!(
        protocol::call_ready(&journal, participant, &published, Some(&ssh.identity))
            .await
            .is_err()
    );
    ygg::knowledge::store::KnowledgeBackup::verify_restored(&candidate_backup, &candidate).unwrap();
    let text: String = sqlx::query_scalar("SELECT text FROM memories LIMIT 1")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "after fleet abort");
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_fleet_events")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(events, 2);
    assert!(
        sqlx::query("DELETE FROM knowledge_fleet_events")
            .execute(&f.pool)
            .await
            .is_err()
    );
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and OpenSSH; two-host post-activation recovery"]
async fn native_partial_fleet_finalization_preserves_writes_on_resume() {
    partial_fleet_finalization_fixture(false, false, false).await;
}
#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and OpenSSH; complete reverse fence and resume"]
async fn native_fleet_reverse_fence_resumes_without_recreating_host_evidence() {
    partial_fleet_finalization_fixture(true, false, false).await;
}
#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and OpenSSH; atomic current-data SQL return"]
async fn native_fleet_sql_return_is_atomic_and_retry_preserves_later_writes() {
    partial_fleet_finalization_fixture(true, true, false).await;
}
#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and OpenSSH; cancel mixed fenced and unfenced hosts"]
async fn native_fleet_cancellation_restores_mixed_hosts_without_fabricating_fences() {
    partial_fleet_finalization_fixture(false, false, true).await;
}
async fn partial_fleet_finalization_fixture(
    complete_reverse: bool,
    return_sql: bool,
    cancel_early: bool,
) {
    use ygg::knowledge::{
        document::digest,
        fleet::{journal::Journal, plan::ValidatedPlan},
        store::KnowledgeStore,
    };
    let server = Server::new();
    let mut first = Fixture::new(&server).await;
    if return_sql {
        // Install before backup so fault injection preserves schema parity.
        sqlx::raw_sql("CREATE FUNCTION public.test_fail_return() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('ygg.test_fail_return',true) = 'on' THEN RAISE EXCEPTION 'injected return failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER test_fail_return BEFORE INSERT ON public.knowledge_recovery_events FOR EACH ROW EXECUTE FUNCTION public.test_fail_return();")
            .execute(&first.pool).await.unwrap();
    }
    let config = ygg::config::database::DeploymentConfig::load(first.env.clone()).unwrap();
    KnowledgeStore::open(&config.knowledge_dir, true).unwrap();
    let identities =
        ygg::knowledge::identity::IdentityRegistry::open(&config.knowledge_policy_dir, true)
            .unwrap()
            .initialize(true)
            .unwrap();
    first.plan.mappings.corpus_id = identities.corpus_id;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut env = first.env.clone();
    for (key, name) in [
        ("YGG_CONFIG_DIR", "config"),
        ("YGG_DATA_DIR", "data"),
        ("YGG_KNOWLEDGE_DIR", "corpus"),
        ("YGG_KNOWLEDGE_POLICY_DIR", "policy"),
    ] {
        env.insert(key.into(), root.join(name).display().to_string());
    }
    let second = Fixture {
        pool: first.pool.clone(),
        temp,
        env,
        plan: serde_json::from_value(serde_json::to_value(&first.plan).unwrap()).unwrap(),
        journal: root.join("journal"),
    };
    let config2 = ygg::config::database::DeploymentConfig::load(second.env.clone()).unwrap();
    KnowledgeStore::open(&config2.knowledge_dir, true).unwrap();
    KnowledgeStore::open(&config2.knowledge_policy_dir, true).unwrap();
    std::fs::write(
        config2.knowledge_policy_dir.join("identity.json"),
        serde_json::to_vec(&identities).unwrap(),
    )
    .unwrap();
    let mut backups = Vec::new();
    for (fixture, config) in [(&first, &config), (&second, &config2)] {
        let path = fixture.temp.path().canonicalize().unwrap().join("backup");
        let manifest = ygg::db::deployment_backup::create(config, &path, Some(&server.bin), None)
            .await
            .unwrap();
        backups.push(serde_json::json!({"path":path,"manifest_sha256":digest(&serde_json::to_vec(&manifest).unwrap())}));
    }
    let ssh1 = ParticipantSsh::new(&first).await;
    let ssh2 = ParticipantSsh::new(&second).await;
    let participants = [Uuid::new_v4(), Uuid::new_v4()];
    let hosts = [(&config, &ssh1), (&config2, &ssh2)].into_iter().enumerate().map(|(i,(config,ssh))| serde_json::json!({
        "id":participants[i],"name":format!("host-{i}"),"protocol":1,"endpoint":&ssh.endpoint,
        "corpus":config.knowledge_dir.canonicalize().unwrap(),"policy":config.knowledge_policy_dir.canonicalize().unwrap(),
        "identities":&identities,"backup":&backups[i],"knowledge_writers_stopped":true,"external_editors_stopped":true
    })).collect::<Vec<_>>();
    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    let seed = first.temp.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "knowledge"]);
    std::fs::write(seed.join("README.txt"), "retained remote metadata\n").unwrap();
    git(&seed, &["add", "README.txt"]);
    git(&seed, &["commit", "-m", "initial"]);
    let remote = first.temp.path().join("remote.git");
    git(
        first.temp.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    git(
        &seed,
        &[
            "push",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/knowledge",
        ],
    );
    let base = git(&seed, &["rev-parse", "HEAD"]);

    let client_key = std::fs::read(ssh2.identity.with_extension("pub")).unwrap();
    let rejected_key = std::fs::read(ssh1.identity.with_extension("pub")).unwrap();
    std::fs::write(ssh1.identity.with_extension("pub"), &client_key).unwrap();
    let plan = ValidatedPlan::parse(&serde_json::json!({
        "version":1,"operation":Uuid::new_v4(),"source_generation":1,"mappings":&first.plan.mappings,"agents":[],
        "shared":{"version":1,"remote":remote,"branch":"knowledge"},"expected_remote_commit":base,
        "source_backup":backups[0],"all_participating_hosts_listed":true,"schema_changes_stopped":true,
        "session_preserving_endpoint":true,"remote_writers_stopped":true,"participants":hosts
    }).to_string()).unwrap();
    let hash = plan.registration().request_sha256.clone();
    let journal = Journal::prepare(&first.journal, plan).unwrap();
    let active = journal
        .activate_hosts(&config, &first.pool, Some(&ssh2.identity))
        .await
        .unwrap();
    assert_eq!(first.marker().await, (3, "okf".into()));
    // Make the FIRST host unreachable so the coordinator must continue to host 2.
    std::fs::write(ssh1.identity.with_extension("pub"), rejected_key).unwrap();
    let error = journal
        .finalize_hosts(&config, &first.pool, Some(&ssh2.identity))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("hosts still require finalization"));
    assert!(!first.journal.join("fleet-finalized.json").exists());
    assert!(
        !first
            .journal
            .join(format!("finalized-{}.json", participants[0]))
            .exists()
    );
    let second_receipt = first
        .journal
        .join(format!("finalized-{}.json", participants[1]));
    let acknowledged = std::fs::read(&second_receipt).unwrap();
    for (cfg, phase) in [
        (&config, ygg::knowledge::runtime::Phase::Fenced),
        (&config2, ygg::knowledge::runtime::Phase::Okf),
    ] {
        let binding: ygg::knowledge::runtime::Binding = serde_json::from_slice(
            &std::fs::read(cfg.knowledge_policy_dir.join("runtime.json")).unwrap(),
        )
        .unwrap();
        assert!(binding.phase == phase);
    }
    let refused = first
        .command()
        .args([
            "remember",
            "unselected host must not write",
            "--global",
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(!refused.status.success());
    success(
        second
            .command()
            .args([
                "remember",
                "write during partial fleet finalization",
                "--global",
                "--json",
            ])
            .output()
            .await
            .unwrap(),
    );
    let shared = ygg::knowledge::shared::SharedGit::open(
        &config2.knowledge_dir,
        journal.plan().unwrap().plan().shared.clone(),
    )
    .unwrap();
    let advanced = shared.refresh().unwrap();
    assert_ne!(advanced.commit, active.publication.commit);
    drop(shared);
    drop(journal);
    // Repair only disposable SSH authorization and resume the exact journal.
    std::fs::write(ssh1.identity.with_extension("pub"), client_key).unwrap();
    let journal = Journal::resume(&first.journal, &hash).unwrap();
    assert_eq!(
        journal
            .finalize_hosts(&config, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap(),
        active
    );
    assert_eq!(std::fs::read(&second_receipt).unwrap(), acknowledged);
    assert!(first.journal.join("fleet-finalized.json").exists());
    assert!(
        first
            .journal
            .join(format!("finalized-{}.json", participants[0]))
            .exists()
    );
    let shared = ygg::knowledge::shared::SharedGit::open(
        &config.knowledge_dir,
        journal.plan().unwrap().plan().shared.clone(),
    )
    .unwrap();
    let current = shared.refresh().unwrap();
    assert_eq!(current.commit, advanced.commit);
    assert_eq!(current.files, advanced.files);
    assert!(current.files.values().any(|bytes| {
        String::from_utf8_lossy(bytes).contains("write during partial fleet finalization")
    }));
    drop(shared);
    success(
        first
            .command()
            .args([
                "remember",
                "write after fleet recovery",
                "--global",
                "--json",
            ])
            .output()
            .await
            .unwrap(),
    );
    assert_eq!(
        journal
            .finalize_hosts(&config, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap(),
        active
    );
    let shared = ygg::knowledge::shared::SharedGit::open(
        &config2.knowledge_dir,
        journal.plan().unwrap().plan().shared.clone(),
    )
    .unwrap();
    let current = shared.refresh().unwrap();
    for text in [
        "write during partial fleet finalization",
        "write after fleet recovery",
    ] {
        assert!(
            current
                .files
                .values()
                .any(|bytes| String::from_utf8_lossy(bytes).contains(text))
        );
    }
    assert!(
        !current
            .files
            .values()
            .any(|bytes| String::from_utf8_lossy(bytes).contains("unselected host must not write"))
    );
    assert!(
        sqlx::query("UPDATE memories SET text='forbidden'")
            .execute(&first.pool)
            .await
            .is_err()
    );
    use ygg::knowledge::fleet::rollback::RollbackPlan;
    let forward = journal.plan().unwrap();
    let request = serde_json::json!({
        "version":1,"operation":Uuid::new_v4(),"forward_operation":forward.plan().operation,
        "forward_request_sha256":forward.registration().request_sha256,
        "activation_sha256":active.readiness_sha256,"source_generation":3,
        "expected_remote_commit":current.commit,"all_participating_hosts_listed":true,
        "schema_changes_stopped":true,"session_preserving_endpoint":true,"remote_writers_stopped":true,
        "participants":participants.map(|id| serde_json::json!({"id":id,"knowledge_writers_stopped":true,"external_editors_stopped":true}))
    });
    for field in [
        "all_participating_hosts_listed",
        "schema_changes_stopped",
        "session_preserving_endpoint",
        "remote_writers_stopped",
    ] {
        let mut invalid = request.clone();
        invalid[field] = false.into();
        assert!(RollbackPlan::parse(forward, &invalid.to_string()).is_err());
    }
    let mut invalid = request.clone();
    invalid["participants"][0]["knowledge_writers_stopped"] = false.into();
    assert!(RollbackPlan::parse(forward, &invalid.to_string()).is_err());
    invalid = request.clone();
    invalid["participants"][0]["id"] = serde_json::json!(Uuid::new_v4());
    assert!(RollbackPlan::parse(forward, &invalid.to_string()).is_err());
    invalid = request.clone();
    invalid["source_generation"] = 1.into();
    assert!(RollbackPlan::parse(forward, &invalid.to_string()).is_err());
    invalid = request.clone();
    invalid["forward_request_sha256"] = "0".repeat(64).into();
    assert!(RollbackPlan::parse(forward, &invalid.to_string()).is_err());
    invalid = request.clone();
    invalid["activation_sha256"] = "0".repeat(64).into();
    assert!(
        RollbackPlan::parse(forward, &invalid.to_string())
            .unwrap()
            .register(&first.pool)
            .await
            .is_err()
    );
    let reverse = RollbackPlan::parse(forward, &request.to_string()).unwrap();
    // Test the digest constraint on an admissible generation before reserving it.
    let bad_hash = sqlx::query("INSERT INTO knowledge_fleet_rollbacks(operation_id,forward_operation_id,database_id,source_generation,request_sha256,request_json) VALUES($1,$2,$3,3,$4,$5)")
        .bind(Uuid::new_v4()).bind(forward.plan().operation).bind(forward.plan().mappings.database_id)
        .bind("0".repeat(64)).bind(reverse.bytes()).execute(&first.pool).await.unwrap_err();
    assert_eq!(
        bad_hash
            .as_database_error()
            .and_then(|e| e.code())
            .as_deref(),
        Some("23514")
    );
    let mut contender = request.clone();
    contender["operation"] = serde_json::json!(Uuid::new_v4());
    let contender = RollbackPlan::parse(forward, &contender.to_string()).unwrap();
    let competing_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&first.env["DATABASE_URL"])
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        reverse.register(&first.pool),
        contender.register(&competing_pool)
    );
    competing_pool.close().await;
    assert_ne!(
        a.is_ok(),
        b.is_ok(),
        "one reverse reservation must own the active generation"
    );
    let winner = if a.is_ok() { &reverse } else { &contender };
    winner.register(&first.pool).await.unwrap();
    // Exact request bytes are part of recovery identity, including whitespace.
    assert!(
        RollbackPlan::parse(forward, &format!("{}\n", winner.bytes()))
            .unwrap()
            .register(&first.pool)
            .await
            .is_err()
    );
    let mut changed: serde_json::Value = serde_json::from_str(winner.bytes()).unwrap();
    changed["expected_remote_commit"] = active.publication.commit.clone().into();
    assert!(
        RollbackPlan::parse(forward, &changed.to_string())
            .unwrap()
            .register(&first.pool)
            .await
            .is_err()
    );
    assert_eq!(first.marker().await, (3, "okf".into()));
    let stored: (String, String) = sqlx::query_as(
        "SELECT request_sha256,request_json FROM knowledge_fleet_rollbacks WHERE operation_id=$1",
    )
    .bind(winner.operation())
    .fetch_one(&first.pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        (winner.sha256().to_owned(), winner.bytes().to_owned())
    );
    for mutation in [
        "UPDATE knowledge_fleet_rollbacks SET request_json=request_json",
        "DELETE FROM knowledge_fleet_rollbacks",
        "TRUNCATE knowledge_fleet_rollbacks",
    ] {
        assert!(sqlx::query(mutation).execute(&first.pool).await.is_err());
    }
    // Even a granted runtime INSERT must not bypass migration-owner checks.
    let mut tx = first.pool.begin().await.unwrap();
    let role = format!("reverse_runtime_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(&format!(
        "GRANT INSERT ON knowledge_fleet_rollbacks TO {role}"
    ))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    let denied = sqlx::query("INSERT INTO knowledge_fleet_rollbacks(operation_id,forward_operation_id,database_id,source_generation,request_sha256,request_json) VALUES($1,$2,$3,3,$4,$5)")
        .bind(winner.operation()).bind(forward.plan().operation).bind(forward.plan().mappings.database_id)
        .bind(winner.sha256()).bind(winner.bytes()).execute(&mut *tx).await.unwrap_err();
    assert_eq!(
        denied.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501")
    );
    tx.rollback().await.unwrap();
    if !complete_reverse {
        // Reserving a reverse operation is not a storage transition or write fence.
        success(
            second
                .command()
                .args([
                    "remember",
                    "reservation alone does not fence",
                    "--global",
                    "--json",
                ])
                .output()
                .await
                .unwrap(),
        );
    }
    use ygg::knowledge::fleet::protocol::call_rollback_fence;
    let loser = if a.is_ok() { &contender } else { &reverse };
    assert!(
        call_rollback_fence(&journal, loser, participants[0], Some(&ssh2.identity))
            .await
            .is_err()
    );
    let local_intent = config.knowledge_policy_dir.join("local-fence-3.json");
    assert!(!local_intent.exists());
    // Verify the entire selected binding before writing any rollback intent.
    let runtime = config.knowledge_policy_dir.join("runtime.json");
    let original_binding = std::fs::read(&runtime).unwrap();
    let mut changed: serde_json::Value = serde_json::from_slice(&original_binding).unwrap();
    changed["agents"] = serde_json::json!({"independent":Uuid::new_v4()});
    std::fs::write(&runtime, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(
        call_rollback_fence(&journal, winner, participants[0], Some(&ssh2.identity))
            .await
            .is_err()
    );
    assert!(!local_intent.exists());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&runtime).unwrap()).unwrap(),
        changed
    );
    std::fs::write(&runtime, original_binding).unwrap();
    let transport_path = config.knowledge_policy_dir.join("shared.json");
    let original_transport = std::fs::read(&transport_path).unwrap();
    let mut changed_transport: serde_json::Value =
        serde_json::from_slice(&original_transport).unwrap();
    changed_transport["branch"] = "independent-branch".into();
    std::fs::write(
        &transport_path,
        serde_json::to_vec(&changed_transport).unwrap(),
    )
    .unwrap();
    assert!(
        call_rollback_fence(&journal, winner, participants[0], Some(&ssh2.identity))
            .await
            .is_err()
    );
    assert!(!local_intent.exists());
    std::fs::write(&transport_path, original_transport).unwrap();
    let first_fence = call_rollback_fence(&journal, winner, participants[0], Some(&ssh2.identity))
        .await
        .unwrap();
    assert_eq!(
        first_fence.fence().coordinator.unwrap().migration_operation,
        winner.operation()
    );
    assert_eq!(first_fence.fence().source_generation, 3);
    if cancel_early {
        let receipt = ygg::knowledge::fleet::rollback::CancellationReceipt {
            operation: winner.operation(),
            request_sha256: winner.sha256().to_owned(),
            database_id: first.plan.mappings.database_id,
            source_generation: 3,
        };
        assert!(
            ygg::knowledge::fleet::protocol::call_rollback_cancel(
                &journal,
                winner,
                participants[0],
                &receipt,
                Some(first_fence.fence()),
                Some(&ssh2.identity)
            )
            .await
            .is_err()
        );
        let restored = journal
            .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap();
        for (index, host) in participants.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(first.journal.join(format!(
                    "rollback-{}-cancel-host-{host}.json",
                    winner.operation()
                )))
                .unwrap(),
            )
            .unwrap();
            assert_eq!(value["fence"].is_null(), index == 1);
        }
        assert_eq!(
            journal
                .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                .await
                .unwrap(),
            restored
        );
        for fixture in [&first, &second] {
            success(
                fixture
                    .command()
                    .args([
                        "remember",
                        "write after mixed cancellation",
                        "--global",
                        "--json",
                    ])
                    .output()
                    .await
                    .unwrap(),
            );
        }
        assert!(!local_intent.exists());
        assert!(
            !config2
                .knowledge_policy_dir
                .join("local-fence-3.json")
                .exists()
        );
        assert!(loser.register(&first.pool).await.is_err());
        assert_eq!(
            journal
                .complete_rollback_cancellation(winner, &first.pool, Some(&ssh2.identity))
                .await
                .unwrap(),
            restored
        );
        first.pool.close().await;
        return;
    }
    assert_eq!(
        call_rollback_fence(&journal, winner, participants[0], Some(&ssh2.identity))
            .await
            .unwrap()
            .fence(),
        first_fence.fence()
    );
    assert!(
        !first
            .command()
            .args(["remember", "blocked reverse host", "--global", "--json"])
            .output()
            .await
            .unwrap()
            .status
            .success()
    );
    if !complete_reverse {
        // A local fence does not assert other hosts or SQL have been fenced.
        success(
            second
                .command()
                .args([
                    "remember",
                    "other host still selected",
                    "--global",
                    "--json",
                ])
                .output()
                .await
                .unwrap(),
        );
    }
    call_rollback_fence(&journal, winner, participants[1], Some(&ssh2.identity))
        .await
        .unwrap();
    assert!(
        !second
            .command()
            .args(["remember", "blocked reverse host two", "--global", "--json"])
            .output()
            .await
            .unwrap()
            .status
            .success()
    );
    assert_eq!(first.marker().await, (3, "okf".into()));
    assert!(
        sqlx::query("UPDATE memories SET text='no SQL activation yet'")
            .execute(&first.pool)
            .await
            .is_err()
    );
    if !complete_reverse {
        let rejected = journal
            .fence_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap_err();
        assert!(format!("{rejected:#}").contains("remote changed from rollback request"));
        assert_eq!(first.marker().await, (3, "okf".into()));
        // The cancellation barrier waits for already admitted host operations.
        let mut lease_connection = sqlx::PgConnection::connect_with(&first.pool.connect_options())
            .await
            .unwrap();
        let mut observer = sqlx::PgConnection::connect_with(&first.pool.connect_options())
            .await
            .unwrap();
        let mut held = lease_connection.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock_shared(1497843531,1)")
            .execute(&mut *held)
            .await
            .unwrap();
        let cancellation = {
            let pending = winner.begin_cancellation(&first.pool);
            tokio::pin!(pending);
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                tokio::select! {
                    result = &mut pending => panic!("cancellation passed an admitted host: {result:?}"),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                }
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid=1497843531 AND objid=1 AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))")
                    .fetch_one(&mut observer).await.unwrap();
                if waiting {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "cancellation did not reach generation lease"
                );
            }
            held.commit().await.unwrap();
            pending.await.unwrap()
        };
        lease_connection.close().await.unwrap();
        observer.close().await.unwrap();
        assert_eq!(cancellation.operation, winner.operation());
        assert_eq!(cancellation.request_sha256, winner.sha256());
        assert_eq!(
            winner.begin_cancellation(&first.pool).await.unwrap(),
            cancellation
        );
        assert_eq!(first.marker().await, (3, "okf".into()));
        assert!(winner.register(&first.pool).await.is_err());
        assert!(
            loser.register(&first.pool).await.is_err(),
            "reservation released before host recovery"
        );
        let changed = ygg::knowledge::fleet::rollback::RollbackPlan::parse(
            journal.plan().unwrap(),
            &format!("{}\n", winner.bytes()),
        )
        .unwrap();
        assert!(changed.begin_cancellation(&first.pool).await.is_err());
        for (id, host_config) in participants.iter().zip([&config, &config2]) {
            let path = host_config.knowledge_policy_dir.join("runtime.json");
            let saved = std::fs::read(&path).unwrap();
            assert!(
                call_rollback_fence(&journal, winner, *id, Some(&ssh2.identity))
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), saved);
        }
        for mutation in [
            "UPDATE knowledge_fleet_rollback_cancellations SET request_sha256=request_sha256",
            "DELETE FROM knowledge_fleet_rollback_cancellations",
            "TRUNCATE knowledge_fleet_rollback_cancellations",
        ] {
            assert!(sqlx::query(mutation).execute(&first.pool).await.is_err());
        }
        // The SQL receipt guard also refuses a cancelled reverse request.
        let hosts = std::fs::read_to_string(
            first
                .journal
                .join(format!("rollback-{}-hosts.json", winner.operation())),
        )
        .unwrap();
        let mut tx = first.pool.begin().await.unwrap();
        sqlx::query("UPDATE knowledge_storage SET generation=4,backend='fenced' WHERE singleton")
            .execute(&mut *tx)
            .await
            .unwrap();
        let failure = sqlx::query("INSERT INTO knowledge_fleet_rollback_fences(operation_id,generation,hosts_sha256,hosts_json,remote_commit) VALUES($1,4,$2,$3,$4)")
            .bind(winner.operation()).bind(digest(hosts.as_bytes())).bind(&hosts).bind(winner.expected_remote_commit()).execute(&mut *tx).await.unwrap_err();
        assert!(
            failure.to_string().contains("rollback operation cancelled"),
            "{failure}"
        );
        tx.rollback().await.unwrap();
        assert_eq!(first.marker().await, (3, "okf".into()));
        let prefix = format!("rollback-{}", winner.operation());
        let authorized = std::fs::read(ssh1.identity.with_extension("pub")).unwrap();
        std::fs::write(ssh1.identity.with_extension("pub"), b"").unwrap();
        assert!(
            journal
                .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        assert!(local_intent.exists());
        assert!(
            !config2
                .knowledge_policy_dir
                .join("local-fence-3.json")
                .exists()
        );
        success(
            second
                .command()
                .args([
                    "remember",
                    "write during rollback cancellation",
                    "--global",
                    "--json",
                ])
                .output()
                .await
                .unwrap(),
        );
        std::fs::write(ssh1.identity.with_extension("pub"), authorized).unwrap();
        // Known host evidence may not be replaced with a never-fenced acknowledgement.
        let original_intent = std::fs::read(&local_intent).unwrap();
        std::fs::remove_file(&local_intent).unwrap();
        assert!(
            journal
                .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        let cancel_intent = config.knowledge_policy_dir.join(format!(
            "local-rollback-cancel-{}-intent.json",
            winner.operation()
        ));
        assert!(!cancel_intent.exists());
        std::fs::write(&local_intent, &original_intent).unwrap();
        let fenced_runtime = std::fs::read(&runtime).unwrap();
        std::fs::write(&runtime, b"independent selection").unwrap();
        assert!(
            journal
                .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&runtime).unwrap(), b"independent selection");
        std::fs::write(&runtime, &fenced_runtime).unwrap();
        let restored = journal
            .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap();
        success(
            first
                .command()
                .args([
                    "remember",
                    "write after rollback cancellation",
                    "--global",
                    "--json",
                ])
                .output()
                .await
                .unwrap(),
        );
        let remote_after = git(&remote, &["rev-parse", "refs/heads/knowledge"]);
        let done = config.knowledge_policy_dir.join(format!(
            "local-rollback-cancel-{}-done.json",
            winner.operation()
        ));
        // Reconstruct each persisted interruption boundary using only fixture files.
        for phase in 0..3 {
            if phase == 0 {
                std::fs::write(&runtime, &fenced_runtime).unwrap();
            }
            if phase <= 1 {
                std::fs::write(&local_intent, &original_intent).unwrap();
            }
            std::fs::remove_file(&done).unwrap();
            assert_eq!(
                journal
                    .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                    .await
                    .unwrap(),
                restored
            );
            assert!(!local_intent.exists());
        }
        let retained_intent = std::fs::read(&cancel_intent).unwrap();
        std::fs::remove_file(&cancel_intent).unwrap();
        assert!(
            journal
                .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        assert!(!cancel_intent.exists());
        std::fs::write(&cancel_intent, retained_intent).unwrap();
        std::fs::write(&local_intent, b"another operation").unwrap();
        assert!(
            journal
                .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&local_intent).unwrap(), b"another operation");
        std::fs::remove_file(&local_intent).unwrap();
        std::fs::remove_file(first.journal.join(format!("{prefix}-restored.json"))).unwrap();
        drop(journal);
        let journal = Journal::resume(&first.journal, &hash).unwrap();
        assert_eq!(
            journal
                .restore_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                .await
                .unwrap(),
            restored
        );
        assert_eq!(
            git(&remote, &["rev-parse", "refs/heads/knowledge"]),
            remote_after
        );
        for (id, host_config) in participants.iter().zip([&config, &config2]) {
            assert!(
                call_rollback_fence(&journal, winner, *id, Some(&ssh2.identity))
                    .await
                    .is_err()
            );
            assert!(
                !host_config
                    .knowledge_policy_dir
                    .join("local-fence-3.json")
                    .exists()
            );
        }
        assert!(
            loser.register(&first.pool).await.is_err(),
            "local acknowledgements alone released SQL reservation"
        );
        let restored_path = first.journal.join(format!("{prefix}-restored.json"));
        let hosts = std::fs::read_to_string(&restored_path).unwrap();
        let mut incomplete: serde_json::Value = serde_json::from_str(&hosts).unwrap();
        incomplete["participants"].as_array_mut().unwrap().pop();
        let incomplete = serde_json::to_string(&incomplete).unwrap();
        let failure = sqlx::query("INSERT INTO knowledge_fleet_rollback_completions(operation_id,hosts_sha256,hosts_json) VALUES($1,$2,$3)")
            .bind(winner.operation()).bind(digest(incomplete.as_bytes())).bind(&incomplete).execute(&first.pool).await.unwrap_err();
        assert!(
            failure.to_string().contains("participant census differs"),
            "{failure}"
        );
        assert!(loser.register(&first.pool).await.is_err());
        assert_eq!(
            journal
                .complete_rollback_cancellation(winner, &first.pool, Some(&ssh2.identity))
                .await
                .unwrap(),
            restored
        );
        for mutation in [
            "UPDATE knowledge_fleet_rollback_completions SET hosts_json=hosts_json",
            "DELETE FROM knowledge_fleet_rollback_completions",
            "TRUNCATE knowledge_fleet_rollback_completions",
        ] {
            assert!(sqlx::query(mutation).execute(&first.pool).await.is_err());
        }
        // Once SQL completion commits, missing local proof is never recreated.
        let done_bytes = std::fs::read(&done).unwrap();
        std::fs::remove_file(&done).unwrap();
        assert!(
            ygg::knowledge::fleet::protocol::call_rollback_cancel(
                &journal,
                winner,
                participants[0],
                &cancellation,
                Some(first_fence.fence()),
                Some(&ssh2.identity)
            )
            .await
            .is_err()
        );
        assert!(!done.exists());
        std::fs::remove_file(&restored_path).unwrap();
        std::fs::remove_file(first.journal.join(format!("{prefix}-cancelled.json"))).unwrap();
        assert_eq!(
            journal
                .complete_rollback_cancellation(winner, &first.pool, None)
                .await
                .unwrap(),
            restored
        );
        assert!(
            !done.exists(),
            "completed coordinator retry contacted hosts or recreated their proof"
        );
        std::fs::write(&done, done_bytes).unwrap();
        // Fresh operations keep old history and compete for one active reservation.
        let mut next: serde_json::Value = serde_json::from_str(winner.bytes()).unwrap();
        next["operation"] = serde_json::json!(Uuid::new_v4());
        next["expected_remote_commit"] = serde_json::json!(remote_after);
        let next_a = ygg::knowledge::fleet::rollback::RollbackPlan::parse(
            journal.plan().unwrap(),
            &next.to_string(),
        )
        .unwrap();
        next["operation"] = serde_json::json!(Uuid::new_v4());
        let next_b = ygg::knowledge::fleet::rollback::RollbackPlan::parse(
            journal.plan().unwrap(),
            &next.to_string(),
        )
        .unwrap();
        let other_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with((*first.pool.connect_options()).clone())
            .await
            .unwrap();
        let (a, b) = tokio::join!(next_a.register(&first.pool), next_b.register(&other_pool));
        assert_ne!(
            a.is_ok(),
            b.is_ok(),
            "fresh reservations were not exclusive: {a:?} {b:?}"
        );
        let admitted = if a.is_ok() { &next_a } else { &next_b };
        let rejected = if a.is_ok() { &next_b } else { &next_a };
        let raw = sqlx::query("INSERT INTO knowledge_fleet_rollbacks(operation_id,forward_operation_id,database_id,source_generation,request_sha256,request_json) VALUES($1,$2,$3,3,$4,$5)")
            .bind(rejected.operation()).bind(journal.plan().unwrap().plan().operation).bind(first.plan.mappings.database_id)
            .bind(rejected.sha256()).bind(rejected.bytes()).execute(&other_pool).await.unwrap_err();
        assert!(
            raw.to_string().contains("another rollback operation owns"),
            "{raw}"
        );
        other_pool.close().await;
        assert!(winner.register(&first.pool).await.is_err());
        let new_fence =
            call_rollback_fence(&journal, admitted, participants[0], Some(&ssh2.identity))
                .await
                .unwrap();
        assert_ne!(new_fence.fence().operation, first_fence.fence().operation);
        assert_eq!(
            new_fence.fence().coordinator.unwrap().migration_operation,
            admitted.operation()
        );
        let new_intent = std::fs::read(&local_intent).unwrap();
        let new_runtime = std::fs::read(&runtime).unwrap();
        ygg::knowledge::fleet::protocol::call_rollback_cancel(
            &journal,
            winner,
            participants[0],
            &cancellation,
            Some(first_fence.fence()),
            Some(&ssh2.identity),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&local_intent).unwrap(), new_intent);
        assert_eq!(std::fs::read(&runtime).unwrap(), new_runtime);
        let new_global = journal
            .fence_rollback_hosts(admitted, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap();
        assert_eq!(new_global.remote_commit, remote_after);
        assert_eq!(first.marker().await, (4, "fenced".into()));
        // Historical completed cancellation is read-only even after SQL advances.
        ygg::knowledge::fleet::protocol::call_rollback_cancel(
            &journal,
            winner,
            participants[0],
            &cancellation,
            Some(first_fence.fence()),
            Some(&ssh2.identity),
        )
        .await
        .unwrap();
        assert_eq!(
            journal
                .complete_rollback_cancellation(winner, &first.pool, None)
                .await
                .unwrap(),
            restored
        );
        assert_eq!(std::fs::read(&local_intent).unwrap(), new_intent);
        assert_eq!(std::fs::read(&runtime).unwrap(), new_runtime);
        assert_eq!(first.marker().await, (4, "fenced".into()));
        first.pool.close().await;
        return;
    }
    sqlx::query("SET default_transaction_isolation='repeatable read'")
        .execute(&first.pool)
        .await
        .unwrap();
    winner
        .fence_host(
            journal.plan().unwrap(),
            &config,
            participants[0],
            &first.pool,
        )
        .await
        .unwrap();
    sqlx::query("SET default_transaction_isolation='read committed'")
        .execute(&first.pool)
        .await
        .unwrap();
    // The SQL receipt rejects an incomplete census even under migration-owner
    // authority; the failed transaction rolls back its temporary marker change.
    let mut tx = first.pool.begin().await.unwrap();
    sqlx::query("UPDATE knowledge_storage SET generation=4,backend='fenced' WHERE singleton")
        .execute(&mut *tx)
        .await
        .unwrap();
    let incomplete =
        serde_json::json!({"version":1,"rollback_sha256":winner.sha256(),"participants":[]})
            .to_string();
    assert!(sqlx::query("INSERT INTO knowledge_fleet_rollback_fences(operation_id,generation,hosts_sha256,hosts_json,remote_commit) VALUES($1,4,$2,$3,$4)")
        .bind(winner.operation()).bind(digest(incomplete.as_bytes())).bind(&incomplete).bind(winner.expected_remote_commit()).execute(&mut *tx).await.is_err());
    tx.rollback().await.unwrap();
    let authorized = std::fs::read(ssh1.identity.with_extension("pub")).unwrap();
    std::fs::write(ssh1.identity.with_extension("pub"), b"").unwrap();
    assert!(
        journal
            .fence_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
            .await
            .is_err()
    );
    assert_eq!(first.marker().await, (3, "okf".into()));
    let prefix = format!("rollback-{}", winner.operation());
    assert!(
        first
            .journal
            .join(format!("{prefix}-host-{}.json", participants[1]))
            .exists()
    );
    assert!(!first.journal.join(format!("{prefix}-hosts.json")).exists());
    std::fs::write(ssh1.identity.with_extension("pub"), authorized).unwrap();
    use sqlx::Connection;
    let mut lease_connection = sqlx::PgConnection::connect_with(&first.pool.connect_options())
        .await
        .unwrap();
    let mut observer = sqlx::PgConnection::connect_with(&first.pool.connect_options())
        .await
        .unwrap();
    clients::register(&mut lease_connection).await.unwrap();
    clients::register(&mut observer).await.unwrap();
    let mut held = lease_connection.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared(1497843531,1)")
        .execute(&mut *held)
        .await
        .unwrap();
    let fenced = {
        let pending = journal.fence_rollback_hosts(winner, &first.pool, Some(&ssh2.identity));
        tokio::pin!(pending);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            tokio::select! {
                result = &mut pending => panic!("reverse fence finished before selected SQL lease drained: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid=1497843531 AND objid=1 AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))")
                .fetch_one(&mut observer).await.unwrap();
            if waiting {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "reverse coordinator did not reach generation lease"
            );
        }
        held.commit().await.unwrap();
        pending.await.unwrap()
    };
    lease_connection.close().await.unwrap();
    observer.close().await.unwrap();
    assert_eq!(fenced.generation, 4);
    assert!(winner.begin_cancellation(&first.pool).await.is_err());
    let cancellations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM knowledge_fleet_rollback_cancellations")
            .fetch_one(&first.pool)
            .await
            .unwrap();
    assert_eq!(cancellations, 0);
    assert_eq!(fenced.remote_commit, winner.expected_remote_commit());
    assert_eq!(first.marker().await, (4, "fenced".into()));
    let stored: (String, String) = sqlx::query_as(
        "SELECT hosts_sha256,hosts_json FROM knowledge_fleet_rollback_fences WHERE operation_id=$1",
    )
    .bind(winner.operation())
    .fetch_one(&first.pool)
    .await
    .unwrap();
    assert_eq!(stored.0, digest(stored.1.as_bytes()));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored.1).unwrap()["participants"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    std::fs::remove_file(first.journal.join(format!("{prefix}-fenced.json"))).unwrap();
    drop(journal);
    let journal = Journal::resume(&first.journal, &hash).unwrap();
    assert_eq!(
        journal
            .fence_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap(),
        fenced
    );
    // Global commit may not recreate a lost local intent or selection.
    for path in [&local_intent, &runtime] {
        let retained = std::fs::read(path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(
            journal
                .fence_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        assert!(!path.exists());
        assert_eq!(first.marker().await, (4, "fenced".into()));
        std::fs::write(path, retained).unwrap();
    }
    assert_eq!(
        journal
            .fence_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap(),
        fenced
    );
    for mutation in [
        "UPDATE knowledge_fleet_rollback_fences SET hosts_json=hosts_json",
        "DELETE FROM knowledge_fleet_rollback_fences",
        "TRUNCATE knowledge_fleet_rollback_fences",
    ] {
        assert!(sqlx::query(mutation).execute(&first.pool).await.is_err());
    }
    assert!(
        sqlx::query("UPDATE memories SET text='still fenced'")
            .execute(&first.pool)
            .await
            .is_err()
    );
    // Capture current shared writes, retaining identical evidence across retries.
    let capture = journal
        .capture_rollback(winner, &config, &first.pool, Some(&ssh2.identity))
        .await
        .unwrap();
    let retained = serde_json::to_value(&capture).unwrap();
    let notes = serde_json::to_string(&capture.candidate.notes).unwrap();
    for text in [
        "write during partial fleet finalization",
        "write after fleet recovery",
    ] {
        assert!(notes.contains(text), "current note missing: {text}");
    }
    assert_eq!(capture.fenced_generation, 4);
    assert_eq!(capture.shared_commit, winner.expected_remote_commit());
    assert_eq!(first.marker().await, (4, "fenced".into()));
    let repeated = journal
        .capture_rollback(winner, &config, &first.pool, Some(&ssh2.identity))
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(repeated).unwrap(), retained);
    // Losing the completion record must recover from the retained paired archives.
    std::fs::remove_file(first.journal.join(format!("{prefix}-capture.json"))).unwrap();
    let repeated = journal
        .capture_rollback(winner, &config, &first.pool, Some(&ssh2.identity))
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(repeated).unwrap(), retained);
    let policy_drift = config
        .knowledge_policy_dir
        .join("independent-capture-policy.json");
    std::fs::write(&policy_drift, b"independent policy").unwrap();
    assert!(
        journal
            .capture_rollback(winner, &config, &first.pool, Some(&ssh2.identity))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&policy_drift).unwrap(), b"independent policy");
    std::fs::remove_file(&policy_drift).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(first.journal.join(format!("{prefix}-capture.json"))).unwrap()
        )
        .unwrap(),
        retained
    );
    let returned = if return_sql {
        let capture_sha256 = digest(&serde_json::to_vec(&capture).unwrap());
        let request_sha256 = digest(
            format!(
                "{}\n{}\n{}",
                winner.sha256(),
                capture_sha256,
                journal.plan().unwrap().plan().source_backup.manifest_sha256
            )
            .as_bytes(),
        );
        let premature = ygg::knowledge::fleet::journal::SqlReturnReceipt {
            operation: winner.operation(),
            generation: 5,
            capture_sha256,
            request_sha256,
        };
        assert!(
            ygg::knowledge::fleet::protocol::call_rollback_deselect(
                &journal,
                winner,
                participants[0],
                &premature,
                Some(&ssh2.identity)
            )
            .await
            .is_err()
        );
        assert!(config.knowledge_policy_dir.join("runtime.json").exists());
        assert!(
            !config
                .knowledge_policy_dir
                .join("local-sql-return-3.json")
                .exists()
        );
        // Fail the last SQL event write: neither imported rows nor authority may commit.
        sqlx::query("SET ygg.test_fail_return = 'on'")
            .execute(&first.pool)
            .await
            .unwrap();
        let failure = journal
            .return_rollback_sql(winner, &config, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap_err();
        assert!(
            format!("{failure:#}").contains("injected return failure"),
            "{failure:#}"
        );
        assert_eq!(first.marker().await, (4, "fenced".into()));
        let texts: Vec<String> = sqlx::query_scalar("SELECT text FROM memories ORDER BY text")
            .fetch_all(&first.pool)
            .await
            .unwrap();
        assert_eq!(texts, vec!["source note"]);
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM knowledge_reverse_receipts WHERE operation_id=$1",
        )
        .bind(winner.operation())
        .fetch_one(&first.pool)
        .await
        .unwrap();
        assert_eq!(count, 0);
        sqlx::query("SET ygg.test_fail_return = 'off'")
            .execute(&first.pool)
            .await
            .unwrap();
        let receipt = journal
            .return_rollback_sql(winner, &config, &first.pool, Some(&ssh2.identity))
            .await
            .unwrap();
        assert_eq!(receipt.generation, 5);
        assert_eq!(first.marker().await, (5, "sql".into()));
        let texts: Vec<String> = sqlx::query_scalar("SELECT text FROM memories ORDER BY text")
            .fetch_all(&first.pool)
            .await
            .unwrap();
        for text in [
            "source note",
            "write during partial fleet finalization",
            "write after fleet recovery",
        ] {
            assert!(texts.iter().any(|t| t == text), "SQL return lost {text}");
        }
        sqlx::query("INSERT INTO memories(text,user_id) VALUES('later SQL write','alice')")
            .execute(&first.pool)
            .await
            .unwrap();
        std::fs::remove_file(first.journal.join(format!("{prefix}-sql.json"))).unwrap();
        Some(receipt)
    } else {
        None
    };
    // Independent remote changes cannot be reset or adopted on a later retry.
    git(
        &seed,
        &["fetch", remote.to_str().unwrap(), "refs/heads/knowledge"],
    );
    git(&seed, &["reset", "--hard", "FETCH_HEAD"]);
    std::fs::write(seed.join("README.txt"), "independent after reverse fence").unwrap();
    git(&seed, &["add", "README.txt"]);
    git(&seed, &["commit", "-m", "independent metadata"]);
    git(
        &seed,
        &[
            "push",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/knowledge",
        ],
    );
    let advanced = git(&seed, &["rev-parse", "HEAD"]);
    if let Some(receipt) = returned {
        drop(journal);
        let journal = Journal::resume(&first.journal, &hash).unwrap();
        let retried = journal
            .return_rollback_sql(winner, &config, &first.pool, None)
            .await
            .unwrap();
        assert_eq!(retried, receipt);
        assert_eq!(first.marker().await, (5, "sql".into()));
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE text='later SQL write')",
        )
        .fetch_one(&first.pool)
        .await
        .unwrap();
        assert!(exists, "committed retry replayed the old import");
        let capture_path = first.journal.join(format!("{prefix}-capture.json"));
        let exact_capture = std::fs::read(&capture_path).unwrap();
        let mut changed_capture = exact_capture.clone();
        changed_capture.push(b' ');
        std::fs::write(&capture_path, &changed_capture).unwrap();
        assert!(
            journal
                .return_rollback_sql(winner, &config, &first.pool, None)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&capture_path).unwrap(), changed_capture);
        assert_eq!(first.marker().await, (5, "sql".into()));
        std::fs::write(&capture_path, exact_capture).unwrap();
        assert_eq!(
            git(&remote, &["rev-parse", "refs/heads/knowledge"]),
            advanced
        );
        // SQL authority alone does not remove either host's local fence.
        for host_config in [&config, &config2] {
            let binding: serde_json::Value = serde_json::from_slice(
                &std::fs::read(host_config.knowledge_policy_dir.join("runtime.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(binding["phase"], "fenced");
        }
        // A failed first host must not prevent the second from returning to SQL.
        let authorized = std::fs::read(ssh1.identity.with_extension("pub")).unwrap();
        std::fs::write(ssh1.identity.with_extension("pub"), b"").unwrap();
        assert!(
            journal
                .deselect_rollback_hosts(winner, &config, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        let runtime1 = config.knowledge_policy_dir.join("runtime.json");
        let runtime2 = config2.knowledge_policy_dir.join("runtime.json");
        assert!(runtime1.exists());
        assert!(!runtime2.exists());
        success(
            second
                .command()
                .args([
                    "remember",
                    "SQL after partial deselection",
                    "--global",
                    "--json",
                ])
                .output()
                .await
                .unwrap(),
        );
        std::fs::write(ssh1.identity.with_extension("pub"), authorized).unwrap();
        // Independently changed selection cannot be removed by a retry.
        let original = std::fs::read(&runtime1).unwrap();
        std::fs::write(&runtime1, b"independent selection").unwrap();
        assert!(
            journal
                .deselect_rollback_hosts(winner, &config, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&runtime1).unwrap(), b"independent selection");
        std::fs::write(&runtime1, original).unwrap();
        assert_eq!(
            journal
                .deselect_rollback_hosts(winner, &config, &first.pool, Some(&ssh2.identity))
                .await
                .unwrap(),
            receipt
        );
        assert!(!runtime1.exists() && !runtime2.exists());
        success(
            first
                .command()
                .args([
                    "remember",
                    "SQL after complete deselection",
                    "--global",
                    "--json",
                ])
                .output()
                .await
                .unwrap(),
        );
        // Missing local evidence after removal must not be manufactured by retries.
        let proof = config2.knowledge_policy_dir.join("local-sql-return-3.json");
        let saved = std::fs::read(&proof).unwrap();
        std::fs::remove_file(&proof).unwrap();
        assert!(
            journal
                .deselect_rollback_hosts(winner, &config, &first.pool, Some(&ssh2.identity))
                .await
                .is_err()
        );
        assert!(!proof.exists() && !runtime2.exists());
        std::fs::write(&proof, saved).unwrap();
        std::fs::remove_file(first.journal.join(format!("{prefix}-deselected.json"))).unwrap();
        drop(journal);
        let journal = Journal::resume(&first.journal, &hash).unwrap();
        assert_eq!(
            journal
                .deselect_rollback_hosts(winner, &config, &first.pool, Some(&ssh2.identity))
                .await
                .unwrap(),
            receipt
        );
        let texts: Vec<String> = sqlx::query_scalar("SELECT text FROM memories")
            .fetch_all(&first.pool)
            .await
            .unwrap();
        for text in [
            "later SQL write",
            "SQL after partial deselection",
            "SQL after complete deselection",
        ] {
            assert!(texts.iter().any(|t| t == text), "retry lost {text}");
        }
        first.pool.close().await;
        return;
    }
    assert!(
        journal
            .capture_rollback(winner, &config, &first.pool, Some(&ssh2.identity))
            .await
            .is_err()
    );
    assert!(
        journal
            .fence_rollback_hosts(winner, &first.pool, Some(&ssh2.identity))
            .await
            .is_err()
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/knowledge"]),
        advanced
    );
    assert_eq!(first.marker().await, (4, "fenced".into()));
    let cache = ygg::knowledge::shared::SharedGit::open(
        &first.journal.join(format!("{prefix}-cache")),
        journal.plan().unwrap().plan().shared.clone(),
    )
    .unwrap();
    assert_eq!(
        cache.cached().unwrap().commit,
        winner.expected_remote_commit(),
        "failed remote validation must not rewrite the committed recovery cache"
    );
    first.pool.close().await;
}

#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN; selected guard freshness after advisory wait"]
async fn native_selected_guard_observes_commit_with_repeatable_read_default() {
    use sqlx::Connection;
    let server = Server::new();
    let f = Fixture::new(&server).await;
    // Isolate the guard contract: an owner publishes a synthetic OKF marker,
    // then fences it while a default-REPEATABLE-READ client waits on the lease.
    sqlx::query(
        "UPDATE knowledge_storage SET backend='fenced',generation=2,corpus_id=$1 WHERE singleton",
    )
    .bind(f.plan.mappings.corpus_id)
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE knowledge_storage SET backend='okf',generation=3 WHERE singleton")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("SET default_transaction_isolation='repeatable read'")
        .execute(&f.pool)
        .await
        .unwrap();
    let mut owner = sqlx::PgConnection::connect_with(&f.pool.connect_options())
        .await
        .unwrap();
    let mut observer = sqlx::PgConnection::connect_with(&f.pool.connect_options())
        .await
        .unwrap();
    let mut change = owner.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)")
        .execute(&mut *change)
        .await
        .unwrap();
    sqlx::query("UPDATE knowledge_storage SET backend='fenced',generation=4 WHERE singleton")
        .execute(&mut *change)
        .await
        .unwrap();
    {
        let selected = ygg::knowledge::guard::selected_transaction(
            &f.pool,
            f.plan.mappings.database_id,
            f.plan.mappings.corpus_id,
            3,
        );
        tokio::pin!(selected);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            tokio::select! {
                result = &mut selected => panic!("selected guard completed while owner held the transition lease: {}", result.is_ok()),
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid=1497843531 AND objid=1 AND mode='ShareLock' AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))")
                .fetch_one(&mut observer).await.unwrap();
            if waiting {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "selected guard did not reach lease"
            );
        }
        change.commit().await.unwrap();
        assert!(
            selected.await.is_err(),
            "guard accepted a pre-wait storage snapshot after the fence committed"
        );
    }
    assert_eq!(f.marker().await, (4, "fenced".into()));
    owner.close().await.unwrap();
    observer.close().await.unwrap();
    f.pool.close().await;
}
