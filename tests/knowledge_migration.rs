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
