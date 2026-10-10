//! Manual process-level benchmark, excluded from normal correctness suites.
#![cfg(any(target_os = "macos", target_os = "linux"))]
use chrono::Utc;
use std::{
    fs,
    path::Path,
    process::{Child, Command},
    time::{Duration, Instant},
};
use uuid::Uuid;
use ygg::knowledge::{
    identity::{GitIdentity, IdentityRegistry},
    matching::Filters,
    service::{Creation, KnowledgeService, RuleInput},
    store::KnowledgeStore,
};

// Ensure failed assertions and barrier timeouts cannot leave benchmark workers
// running after the temporary corpus has been removed.
#[derive(Default)]
struct Workers(Vec<Child>);
impl Drop for Workers {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn service(root: &Path) -> KnowledgeService {
    KnowledgeService::new(
        KnowledgeStore::open(&root.join("bundle"), false).unwrap(),
        IdentityRegistry::open(&root.join("policy"), false).unwrap(),
        "benchmark".into(),
    )
    .unwrap()
}

fn query(service: &KnowledgeService, repo: Uuid) -> (f64, f64, usize) {
    let now = Utc::now();
    let filters = Filters {
        repo: Some(repo),
        rule: Some("rule-0"),
        ..Filters::default()
    };
    let start = Instant::now();
    let rules = service.rules(&filters, now).unwrap();
    assert!(rules.diagnostics.is_empty(), "{:?}", rules.diagnostics);
    assert_eq!(rules.documents.len(), 20);
    let verified = service
        .revalidate_rules(&rules.documents, &filters, now)
        .unwrap();
    assert!(
        verified.diagnostics.is_empty(),
        "{:?}",
        verified.diagnostics
    );
    assert_eq!(verified.documents.len(), 20);
    let mut bytes: usize = verified
        .documents
        .iter()
        .map(|doc| doc.document.body.len())
        .sum();
    let rule_ms = start.elapsed().as_secs_f64() * 1000.;
    let start = Instant::now();
    let notes = service.prime_notes(Some(repo), now).unwrap();
    assert!(notes.diagnostics.is_empty(), "{:?}", notes.diagnostics);
    assert_eq!(notes.documents.len(), 5);
    let verified = service
        .revalidate_notes(&notes.documents, Some(repo), now)
        .unwrap();
    assert!(
        verified.diagnostics.is_empty(),
        "{:?}",
        verified.diagnostics
    );
    assert_eq!(verified.documents.len(), 5);
    bytes += verified
        .documents
        .iter()
        .map(|doc| doc.document.body.len())
        .sum::<usize>();
    (rule_ms, start.elapsed().as_secs_f64() * 1000., bytes)
}

#[test]
#[ignore = "subprocess entry point for okf_process_benchmark"]
fn okf_benchmark_worker() {
    let Some(root) = std::env::var_os("YGG_OKF_BENCH_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let worker = std::env::var("YGG_OKF_BENCH_WORKER").unwrap();
    let repo = fs::read_to_string(root.join("repo"))
        .unwrap()
        .parse()
        .unwrap();
    let service = service(root);
    // Warm each independent process before synchronizing measured requests.
    query(&service, repo);
    fs::write(root.join(format!("ready-{worker}")), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(300);
    while !root.join("go").exists() {
        assert!(Instant::now() < deadline, "benchmark barrier timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    let samples: Vec<_> = (0..3).map(|_| query(&service, repo)).collect();
    fs::write(
        root.join(format!("result-{worker}.json")),
        serde_json::to_vec(&samples).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "manual release benchmark: 10,000 documents and 20 OS processes"]
fn okf_process_benchmark() {
    assert!(!cfg!(debug_assertions), "run with cargo test --release");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let registry = IdentityRegistry::open(&root.join("policy"), true).unwrap();
    registry.initialize(true).unwrap();
    let repo = registry
        .bind(&GitIdentity {
            common_dir: root.canonicalize().unwrap().join("repo.git"),
            origin: None,
        })
        .unwrap();
    let store = KnowledgeStore::open(&root.join("bundle"), true).unwrap();
    let service = KnowledgeService::new(store, registry, "benchmark".into()).unwrap();
    let now = Utc::now();
    let note = service
        .create_note(Some(repo), "Benchmark note.\n".repeat(16), None, now)
        .unwrap();
    let rule = service
        .create_rule(
            RuleInput {
                repo: Some(repo),
                text: "Benchmark rule.\n".repeat(16),
                rule_id: Some("rule-0".into()),
                ..RuleInput::default()
            },
            Creation::ManualActive,
            now,
        )
        .unwrap();
    let corpus = IdentityRegistry::open(&root.join("policy"), false)
        .unwrap()
        .read()
        .unwrap()
        .0
        .corpus_id;
    // Fixture setup is deliberately outside timing. Direct copies avoid measuring
    // 10,000 conditional mutations; each rule receives its own valid activation.
    for index in 2..10_000 {
        let template = if index % 5 == 0 { &rule } else { &note };
        let mut doc = template.document.clone();
        let mut profile = doc.profile().unwrap().unwrap();
        profile.id = Uuid::new_v4();
        if index % 5 == 0 {
            profile.rule_id = Some(format!("rule-{}", (index / 5) % 100));
        }
        doc.set_profile(&profile).unwrap();
        if index % 5 == 0 {
            doc.activate(
                corpus,
                ygg::knowledge::document::ActivationKind::Manual,
                None,
                Some(now),
            )
            .unwrap();
        }
        let key = ygg::knowledge::store::Key::from_document(&doc).unwrap();
        let path = root.join("bundle").join(key.relative_path());
        fs::write(&path, doc.serialize().unwrap()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(root.join("repo"), repo.to_string()).unwrap();
    let mut children = Workers::default();
    for worker in 0..20 {
        children.0.push(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "okf_benchmark_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("YGG_OKF_BENCH_ROOT", root)
                .env("YGG_OKF_BENCH_WORKER", worker.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(300);
    let ready = loop {
        if (0..20).all(|i| root.join(format!("ready-{i}")).exists()) {
            break true;
        }
        if Instant::now() > deadline
            || children
                .0
                .iter_mut()
                .any(|c| c.try_wait().unwrap().is_some())
        {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !ready {
        for child in &mut children.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("benchmark workers failed or timed out before barrier");
    }
    fs::write(root.join("go"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let mut complete = true;
        for child in &mut children.0 {
            match child.try_wait().unwrap() {
                Some(status) => assert!(status.success(), "benchmark worker failed"),
                None => complete = false,
            }
        }
        if complete {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "benchmark measurements timed out"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut samples = Vec::<(f64, f64, usize)>::new();
    for worker in 0..20 {
        samples.extend(
            serde_json::from_slice::<Vec<(f64, f64, usize)>>(
                &fs::read(root.join(format!("result-{worker}.json"))).unwrap(),
            )
            .unwrap(),
        );
    }
    let percentile = |column: usize, percentile: usize| {
        let mut values: Vec<_> = samples
            .iter()
            .map(|s| if column == 0 { s.0 } else { s.1 })
            .collect();
        values.sort_by(f64::total_cmp);
        values[(values.len() * percentile).div_ceil(100) - 1]
    };
    println!(
        "{}",
        serde_json::json!({
            "documents": 10000, "clients": 20, "samples": samples.len(),
            "rules_ms": {"p50": percentile(0, 50), "p95": percentile(0, 95)},
            "prime_notes_ms": {"p50": percentile(1, 50), "p95": percentile(1, 95)},
            "body_bytes_per_sample": samples[0].2,
            "measurement": "warm service selection plus selected-document revalidation; excludes CLI startup, SQL, rendering and telemetry"
        })
    );
}
