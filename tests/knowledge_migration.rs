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
