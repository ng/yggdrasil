//! M7: a fresh installation gets an OKF corpus from `ygg knowledge init` with
//! no legacy mappings, unmapped checkouts bind on first use, and connected task
//! claims resolve repositories registered after cutover.
#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::path::Path;
use uuid::Uuid;
use ygg::models::{
    repo::RepoRepo,
    task::{TaskCreate, TaskKind, TaskRepo},
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
        let database = format!("ygg_boot_{}", Uuid::new_v4().simple());
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

fn app(f: &Fixture, root: &Path, cwd: &Path) -> std::process::Command {
    let mut cmd = okf::app(root, cwd);
    cmd.env("DATABASE_URL", &f.url);
    cmd
}

#[tokio::test]
async fn fresh_install_bootstraps_okf_and_binds_new_checkouts() {
    let f = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("repo")).unwrap();
    okf::git(&root.join("repo"), &["init", "-q"]);
    okf::git(
        &root.join("repo"),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    let result = std::panic::AssertUnwindSafe(async {
        // Legacy tables are gone on a fresh database.
        let legacy: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT to_regclass('public.memories')::text, to_regclass('public.learnings')::text",
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(legacy, (None, None));
        // Uninitialized knowledge is an explicit error, never a silent fallback.
        let out = app(&f, root, &root.join("repo"))
            .args(["remember", "too early"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("ygg init"));

        let created = okf::json(
            app(&f, root, root)
                .args(["knowledge", "init", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(created["created"], true);
        assert_eq!(created["generation"], 3);
        assert_eq!(created["user"], "legacy-user");
        let corpus = Uuid::parse_str(created["corpus_id"].as_str().unwrap()).unwrap();
        let marker: (i64, String, Option<Uuid>) =
            sqlx::query_as("SELECT generation, backend, corpus_id FROM public.knowledge_storage")
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(marker, (3, "okf".into(), Some(corpus)));
        let again = okf::json(
            app(&f, root, root)
                .args(["knowledge", "init", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(again["created"], false);
        assert_eq!(again["generation"], 3);

        // A checkout with no binding binds on first use and reports its
        // portable repository as repo_id.
        let note = okf::json(
            okf::command(root, &root.join("repo"))
                .args(["fresh install note", "--json"])
                .output()
                .unwrap(),
        );
        let portable = Uuid::parse_str(note["repo_id"].as_str().unwrap()).unwrap();
        let listed = okf::json(
            okf::command(root, &root.join("repo"))
                .args(["--list", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(listed["count"], 1);
        let rule = okf::json(
            okf::learn(root)
                .args(["create", "fresh rule", "--file-glob", "src/*.rs", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(rule["repo_id"].as_str().unwrap(), portable.to_string());
        let prime = app(&f, root, &root.join("repo"))
            .arg("prime")
            .output()
            .unwrap();
        assert!(prime.status.success());
        assert!(String::from_utf8_lossy(&prime.stdout).contains("fresh install note"));

        // A repository registered in the database after cutover has no legacy
        // mapping; claims resolve it through its registered checkout.
        let registered = RepoRepo::new(&f.pool)
            .register(
                None,
                "bootfixture",
                "bootfixture",
                Some(root.join("repo").to_str().unwrap()),
            )
            .await
            .unwrap();
        assert_ne!(registered.repo_id, portable);
        let task = TaskRepo::new(&f.pool)
            .create(
                registered.repo_id,
                None,
                TaskCreate {
                    title: "Claim fixture",
                    description: "Edit src/one.rs",
                    kind: TaskKind::Bug,
                    ..TaskCreate::default()
                },
            )
            .await
            .unwrap();
        let out = app(&f, root, &root.join("repo"))
            .args(["task", "claim", &task.task_id.to_string()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("fresh rule"), "{text}");
        assert!(!root.join("data").join("postgres").exists());
    })
    .catch_unwind()
    .await;
    f.cleanup().await;
    result.unwrap();
}
use futures::FutureExt;
