#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::Command,
};
use ygg::knowledge::{
    document::digest, identity::IdentityRegistry, matching::Filters, service::KnowledgeService,
    shared::Config, store::KnowledgeStore,
};

const GENERIC: &str = "---\r\ntype: Design Decision\r\ntitle: A generic decision\r\ncustom: {preserved: [1, two]}\r\n---\r\n  EXACT_GENERIC_BODY Ω\r\n";
fn browse(root: &Path, args: &[&str]) -> std::process::Output {
    okf::app(root, root)
        .args(["knowledge", "browse"])
        .args(args)
        .output()
        .unwrap()
}
fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
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
}
#[test]
fn private_cli_browses_generic_exact_bytes_without_activating_or_contacting_sql() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    let bundle = root.join("bundle");
    std::fs::create_dir(bundle.join("decisions")).unwrap();
    std::fs::write(bundle.join("decisions/example.md"), GENERIC).unwrap();
    let canonical = format!("global/learnings/{}.md", uuid::Uuid::new_v4());
    std::fs::create_dir_all(bundle.join("global/learnings")).unwrap();
    let generic_rule = GENERIC.replace("Design Decision", "Engineering Rule");
    std::fs::write(bundle.join(&canonical), &generic_rule).unwrap();
    let listing = okf::json(browse(root, &["--json"]));
    assert_eq!(listing["documents"].as_array().unwrap().len(), 2);
    assert_eq!(listing["documents"][0]["path"], "decisions/example.md");
    assert!(listing["documents"][0].get("text").is_none());
    assert!(listing["diagnostics"].as_array().unwrap().is_empty());
    let shown = okf::json(browse(root, &["decisions/example.md", "--json"]));
    assert_eq!(shown["documents"][0]["text"], GENERIC);
    assert_eq!(
        shown["documents"][0]["revision"],
        digest(GENERIC.as_bytes())
    );
    let raw = browse(root, &["decisions/example.md"]);
    assert!(raw.status.success());
    assert_eq!(raw.stdout, GENERIC.as_bytes());
    let service = KnowledgeService::new(
        KnowledgeStore::open(&bundle, false).unwrap(),
        IdentityRegistry::open(&root.join("policy"), false).unwrap(),
        "portable-user".into(),
    )
    .unwrap();
    assert!(
        service
            .rules(&Filters::default(), chrono::Utc::now())
            .unwrap()
            .documents
            .is_empty()
    );
    assert_eq!(
        std::fs::read(bundle.join(&canonical)).unwrap(),
        generic_rule.as_bytes()
    );
    assert!(!root.join("data").exists());
}
#[test]
fn unsafe_paths_and_partial_listings_fail_without_hiding_valid_documents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    let bundle = root.join("bundle");
    std::fs::write(bundle.join("valid.md"), GENERIC).unwrap();
    std::fs::write(bundle.join("broken.md"), "not OKF").unwrap();
    std::fs::write(root.join("secret.md"), GENERIC).unwrap();
    std::fs::create_dir(bundle.join(".private")).unwrap();
    std::fs::write(bundle.join(".private/hidden.md"), GENERIC).unwrap();
    symlink(root.join("secret.md"), bundle.join("escape.md")).unwrap();
    symlink(root, bundle.join("outside")).unwrap();
    std::fs::hard_link(root.join("secret.md"), bundle.join("hard.md")).unwrap();
    let fifo = std::ffi::CString::new(bundle.join("pipe.md").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let output = browse(root, &["--json"]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["documents"].as_array().unwrap().len(), 1);
    assert_eq!(report["documents"][0]["path"], "valid.md");
    assert!(report["diagnostics"].as_array().unwrap().len() >= 4);
    for path in [
        "../secret.md",
        "/secret.md",
        ".private/hidden.md",
        "outside/secret.md",
        "escape.md",
        "hard.md",
        "pipe.md",
        "missing.md",
        "valid.md/../valid.md",
        "a\\b.md",
    ] {
        assert!(!browse(root, &[path, "--json"]).status.success(), "{path}");
    }
}
#[test]
fn shared_cli_browses_confirmed_generic_revision_and_marks_outage_cache_stale() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let remote = root.join("remote.git");
    let seed = root.join("seed");
    git(root, &["init", "--bare", remote.to_str().unwrap()]);
    git(root, &["init", "-b", "knowledge", seed.to_str().unwrap()]);
    std::fs::write(seed.join("design.md"), GENERIC).unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "generic OKF"]);
    git(
        &seed,
        &[
            "push",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/knowledge",
        ],
    );
    let (binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    let config = Config {
        version: 1,
        remote: remote.to_str().unwrap().into(),
        branch: "knowledge".into(),
    };
    let selection = root.join("policy/shared.json");
    std::fs::write(&selection, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::set_permissions(&selection, std::fs::Permissions::from_mode(0o600)).unwrap();
    let live = okf::json(browse(root, &["design.md", "--json"]));
    assert_eq!(live["documents"][0]["text"], GENERIC);
    assert_eq!(live["shared_current"], true);
    assert!(live["shared_commit"].as_str().unwrap().len() >= 40);
    let transport = ygg::knowledge::shared::SharedGit::open(&root.join("bundle"), config).unwrap();
    let store = KnowledgeStore::open(&root.join("bundle"), false).unwrap();
    let policy = KnowledgeStore::open(&root.join("policy"), false).unwrap();
    let recovery = store
        .backup_pair_retained(
            &policy,
            &root.join("shared-backup"),
            &root.join("shared-policy-backup"),
        )
        .unwrap();
    let error = transport
        .recovery_snapshot(&recovery)
        .err()
        .expect("generic shared corpus cannot reverse to SQL");
    assert!(
        error.to_string().contains("SQL cannot represent"),
        "{error:#}"
    );
    drop(recovery);
    std::fs::rename(&remote, root.join("offline.git")).unwrap();
    let cached = okf::json(browse(root, &["design.md", "--json"]));
    assert_eq!(cached["documents"], live["documents"]);
    assert_eq!(cached["shared_commit"], live["shared_commit"]);
    assert_eq!(cached["shared_current"], false);
    assert!(!root.join("data").exists());
}

#[test]
fn malformed_bytes_consume_read_budget_and_explicit_reads_remain_independent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("bundle");
    let store = KnowledgeStore::open(&root, true).unwrap();
    let bytes = vec![255u8; ygg::knowledge::document::MAX_DOCUMENT_BYTES];
    for index in 0..64 {
        std::fs::write(root.join(format!("bad-{index:02}.md")), &bytes).unwrap();
    }
    std::fs::write(root.join("last.md"), GENERIC).unwrap();
    let report = store.browse(None).unwrap();
    assert!(report.documents.is_empty());
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.contains("total byte limit"))
    );
    assert_eq!(
        store.browse(Some("last.md")).unwrap().documents[0]
            .text
            .as_deref(),
        Some(GENERIC)
    );
    let too_large = root.join("oversize.md");
    std::fs::File::create(&too_large)
        .unwrap()
        .set_len((bytes.len() + 1) as u64)
        .unwrap();
    assert!(
        store
            .browse(Some("oversize.md"))
            .unwrap_err()
            .to_string()
            .contains("byte limit")
    );
    let deep = (0..33).map(|_| "a").collect::<Vec<_>>().join("/") + ".md";
    assert!(
        store
            .browse(Some(&deep))
            .unwrap_err()
            .to_string()
            .contains("relative bundle path")
    );
}

#[test]
fn generic_documents_are_backed_up_but_cannot_be_omitted_from_sql_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let store = KnowledgeStore::open(&root.join("bundle"), true).unwrap();
    let policy = KnowledgeStore::open(&root.join("policy"), true).unwrap();
    std::fs::write(root.join("bundle/decision.md"), GENERIC).unwrap();
    let retained = store
        .backup_pair_retained(
            &policy,
            &root.join("saved-bundle"),
            &root.join("saved-policy"),
        )
        .unwrap();
    assert_eq!(
        std::fs::read(root.join("saved-bundle/corpus/decision.md")).unwrap(),
        GENERIC.as_bytes()
    );
    assert!(
        retained
            .snapshot()
            .unwrap_err()
            .to_string()
            .contains("SQL cannot represent")
    );
    retained.verify_sources().unwrap();
}
