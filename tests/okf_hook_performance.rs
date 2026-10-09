//! Manual end-to-end SQL-relative edit-hook qualification on disposable data.
#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use futures::FutureExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Stdio},
    sync::Barrier,
    time::{Duration, Instant},
};
use uuid::Uuid;
use ygg::{
    knowledge::{identity::IdentityRegistry, legacy, runtime::UsageSnapshot, store::Key},
    models::{agent::AgentRepo, learning::Learning, memory::Memory, repo::RepoRepo},
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
        let database = format!("ygg_hook_bench_{}", Uuid::new_v4().simple());
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
struct Running(Option<Child>);
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
#[derive(serde::Serialize)]
struct Sample {
    total_ms: f64,
    phases_us: BTreeMap<String, u64>,
}
fn hook(root: &Path, url: &str, worker: usize, expected: &str) -> Sample {
    let session = format!("ygg-bench-{}", Uuid::new_v4());
    let start = Instant::now();
    let mut child = Running(Some(
        okf::app(root, &root.join("repo"))
            .env("DATABASE_URL", url)
            .env("NO_COLOR", "1")
            .env(
                "RUST_LOG",
                if std::env::var_os("YGG_HOOK_BENCH_PHASES").is_some() {
                    "ygg::knowledge::timing=debug"
                } else {
                    "ygg=info"
                },
            )
            .env("YGG_AGENT_NAME", format!("bench-{worker}"))
            .args(["hook", "pre-tool-use"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    child.0.as_mut().unwrap().stdin.take().unwrap().write_all(serde_json::to_string(&serde_json::json!({
        "session_id":session,"tool_name":"Edit","tool_input":{"file_path":format!("src/client-{worker}-{session}.rs")}
    })).unwrap().as_bytes()).unwrap();
    while child.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "hook deadline exceeded"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    let elapsed = start.elapsed().as_secs_f64() * 1000.;
    // Remove only this invocation's legacy receipt, outside measured latency.
    let _ = fs::remove_file(format!("/tmp/ygg/learnings-{session}.seen"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        !text.contains("unavailable") && !text.contains("locked by"),
        "{text}"
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["hookSpecificOutput"]["additionalContext"], expected,
        "missing, duplicate or reordered hook rules"
    );
    let mut phases_us = BTreeMap::new();
    if std::env::var_os("YGG_HOOK_BENCH_PHASES").is_some() {
        // Parse only our static timing fields, after the measured process exits.
        let ansi = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        let plain = ansi.replace_all(&text, "");
        let fields = regex::Regex::new(r#"phase="([a-z_]+)" elapsed_us=(\d+)"#).unwrap();
        for capture in fields.captures_iter(&plain) {
            assert!(
                phases_us
                    .insert(capture[1].to_owned(), capture[2].parse().unwrap())
                    .is_none(),
                "duplicate timing phase in one hook"
            );
        }
        assert!(
            phases_us.contains_key("hook_coordination"),
            "missing timing fields: {plain}"
        );
    }
    Sample {
        total_ms: elapsed,
        phases_us,
    }
}
fn measure(root: &Path, url: &str, expected: &str) -> Vec<Sample> {
    let mut samples = Vec::new();
    // Every phase warms all twenty clients, then measures five synchronized
    // rounds. A fresh session forces real selection/output in each invocation.
    for round in 0..6 {
        let barrier = Barrier::new(20);
        let times = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..20)
                .map(|worker| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        hook(root, url, worker, expected)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().unwrap())
                .collect::<Vec<_>>()
        });
        if round != 0 {
            samples.extend(times);
        }
    }
    samples
}
fn percentile(samples: &[Sample], percentile: usize) -> f64 {
    let mut values: Vec<_> = samples.iter().map(|s| s.total_ms).collect();
    values.sort_by(f64::total_cmp);
    values[(values.len() * percentile).div_ceil(100) - 1]
}
fn write_document(root: &Path, doc: &ygg::knowledge::document::Document) {
    let path = root
        .join("bundle")
        .join(Key::from_document(doc).unwrap().relative_path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(
        path.parent().unwrap().parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(&path, doc.serialize().unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
async fn qualify(f: &Fixture) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = okf::fixture(root);
    let repo = RepoRepo::new(&f.pool)
        .register(
            None,
            "repo",
            "repo",
            Some(root.join("repo").to_str().unwrap()),
        )
        .await
        .unwrap();
    let database: Uuid = sqlx::query_scalar("SELECT database_id FROM public.knowledge_storage")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let registry = IdentityRegistry::open(&root.join("policy"), false).unwrap();
    let (mut identities, revision) = registry.read().unwrap();
    identities.repos[0].databases = BTreeMap::from([(database, [repo.repo_id].into())]);
    registry.replace(&revision, &identities).unwrap();
    binding.mappings.database_id = database;
    binding.mappings.repos = BTreeMap::from([(repo.repo_id, identities.repos[0].id)]);
    binding.agents.clear();
    for worker in 0..20 {
        let name = format!("bench-{worker}");
        let agent = AgentRepo::new(&f.pool, "legacy-user".into())
            .register(&name)
            .await
            .unwrap();
        binding.agents.insert(name, agent.agent_id);
    }
    let mut rules = Vec::new();
    let mut notes = Vec::new();
    let mut expected = Vec::new();
    let now = ygg::knowledge::document::timestamp_now();
    for index in 0..10000 {
        let id = Uuid::new_v4();
        let created = now + chrono::Duration::microseconds(index);
        if index % 5 == 0 {
            let group = (index / 5) % 100;
            let glob = if group == 0 {
                "src/*.rs".into()
            } else {
                format!("other-{group}/*.rs")
            };
            let row = Learning {
                learning_id: id,
                repo_id: Some(repo.repo_id),
                file_glob: Some(glob),
                rule_id: None,
                text: format!("Benchmark rule {index:05}."),
                context: None,
                created_by: None,
                created_at: created,
                applied_count: 0,
                last_applied_at: None,
                scope_tags: serde_json::json!({}),
                status: "active".into(),
                source: "manual".into(),
                approved_at: None,
                approved_by: None,
            };
            if group == 0 {
                expected.push(format!("[ygg learning · src/*.rs] {}", row.text));
            }
            write_document(
                root,
                &legacy::import_learning(&row, "legacy-user", &binding.mappings)
                    .unwrap()
                    .0,
            );
            let mut value = serde_json::to_value(row).unwrap();
            value["user_id"] = "legacy-user".into();
            rules.push(value);
        } else {
            let row = Memory {
                memory_id: id,
                repo_id: Some(repo.repo_id),
                text: format!("Benchmark note {index:05}."),
                created_by: None,
                created_at: created,
            };
            write_document(
                root,
                &legacy::import_note(&row, "legacy-user", &binding.mappings).unwrap(),
            );
            let mut value = serde_json::to_value(row).unwrap();
            value["user_id"] = "legacy-user".into();
            notes.push(value);
        }
    }
    assert_eq!(rules.len(), 2000);
    assert_eq!(notes.len(), 8000);
    assert_eq!(expected.len(), 20);
    sqlx::query("INSERT INTO learnings SELECT * FROM jsonb_populate_recordset(NULL::learnings,$1)")
        .bind(serde_json::json!(rules))
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memories SELECT * FROM jsonb_populate_recordset(NULL::memories,$1)")
        .bind(serde_json::json!(notes))
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::raw_sql("ANALYZE learnings; ANALYZE memories;")
        .execute(&f.pool)
        .await
        .unwrap();
    expected.reverse();
    let expected = expected.join("\n");
    let sql = measure(root, &f.url, &expected);
    // This is fixture selection, not a test of the production cutover workflow.
    let current: Vec<Learning> = sqlx::query_as("SELECT * FROM learnings")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    let mut totals = BTreeMap::new();
    for row in current {
        assert_eq!(
            row.applied_count,
            if row.file_glob.as_deref() == Some("src/*.rs") {
                120
            } else {
                0
            },
            "SQL hook lost a usage observation"
        );
        let usage = legacy::Usage {
            corpus_id: binding.mappings.corpus_id,
            document_id: row.learning_id,
            applied_count: row.applied_count,
            last_applied_at: row.last_applied_at,
        };
        ygg::knowledge::telemetry::Telemetry::new(&f.pool)
            .seed(&usage)
            .await
            .unwrap();
        totals.insert(row.learning_id, usage);
    }
    fs::write(
        root.join("policy/usage-baseline.json"),
        serde_json::to_vec(&UsageSnapshot {
            version: 1,
            corpus_id: binding.mappings.corpus_id,
            totals,
        })
        .unwrap(),
    )
    .unwrap();
    sqlx::query("UPDATE public.knowledge_storage SET backend='fenced',corpus_id=$1,generation=2")
        .bind(binding.mappings.corpus_id)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE public.knowledge_storage SET backend='okf',generation=3")
        .execute(&f.pool)
        .await
        .unwrap();
    binding.generation = 3;
    okf::select(root, &binding);
    let okf = measure(root, &f.url, &expected);
    let observed: i64 = sqlx::query_scalar(
        "SELECT sum(observed_count)::bigint FROM public.knowledge_usage WHERE corpus_id=$1",
    )
    .bind(binding.mappings.corpus_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let version: String = sqlx::query_scalar("SHOW server_version")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let added = percentile(&okf, 95) - percentile(&sql, 95);
    let report = serde_json::json!({"documents":10000,"clients":20,"samples_per_mode":sql.len(),
        "platform":format!("{}-{}",std::env::consts::OS,std::env::consts::ARCH),
        "logical_cpus":std::thread::available_parallelism().unwrap().get(),"postgres":version,
        "okf_observed_applications":observed,"expected_applications":2400,
        "sql_ms":{"p50":percentile(&sql,50),"p95":percentile(&sql,95)},
        "okf_ms":{"p50":percentile(&okf,50),"p95":percentile(&okf,95)},
        "added_p95_ms":added,"limit_ms":50,"passed":added<=50. && observed==2400,
        "measurement":"warm actual pre-tool-use CLI, including startup, matching, revalidation, output, coordination and telemetry; SQL phase then OKF; new session per request; 20 identical rules required",
        "sql_samples_ms":sql.iter().map(|s| s.total_ms).collect::<Vec<_>>(),
        "okf_samples_ms":okf.iter().map(|s| s.total_ms).collect::<Vec<_>>(),
        "phase_diagnostics_enabled":std::env::var_os("YGG_HOOK_BENCH_PHASES").is_some(),
        "phase_note":"Nested phase timings are inclusive and must not be summed; debug tracing adds overhead. Diagnostic runs do not replace uninstrumented qualification.",
        "sql_phase_samples_us":sql.iter().map(|s| &s.phases_us).collect::<Vec<_>>(),
        "okf_phase_samples_us":okf.iter().map(|s| &s.phases_us).collect::<Vec<_>>()});
    println!("{report}");
    if let Some(path) = std::env::var_os("YGG_HOOK_BENCH_REPORT") {
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    assert_eq!(observed, 2400, "OKF hook lost a usage observation");
    assert!(
        added <= 50.,
        "SQL-relative edit-hook added p95 {added:.2} ms exceeds 50 ms"
    );
}
#[tokio::test]
#[ignore = "manual release qualification: isolated DATABASE_URL, 10000 documents, 20 clients"]
async fn sql_relative_edit_hook_p95() {
    assert!(!cfg!(debug_assertions), "run with cargo test --release");
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(qualify(&f))
        .catch_unwind()
        .await;
    f.cleanup().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
