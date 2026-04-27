//! Regression tests for the bench grader's git-log lookup.
//!
//! Both grade.sh scripts replaced an in-loop `git log --all | grep -qF` pipeline
//! (which broke under SIGPIPE in some shell/repo combinations and reported
//! false negatives) with a captured-variable form:
//!
//! ```sh
//! log_out=$(git log --all --pretty=%B 2>/dev/null || true)
//! printf '%s' "$log_out" | grep -qF "$commit_msg"
//! ```
//!
//! These tests pin both scripts to that pattern and exercise the
//! 0/1/many-matching-commits branches end-to-end against a real git repo.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn grader(scenario: &str) -> PathBuf {
    repo_root()
        .join("benches/scenarios")
        .join(scenario)
        .join("grade.sh")
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git invocation failed");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

fn commit_empty(dir: &Path, msg: &str) {
    git(dir, &["commit", "--allow-empty", "-q", "-m", msg]);
}

fn write_doc(dir: &Path, rel: &str, h1: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!("# {h1}\n\nBody.\n\n## Strategies\n\nDetail.\n"),
    )
    .unwrap();
    git(dir, &["add", rel]);
}

fn run_grader(grader: &Path, workdir: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(grader)
        .arg(workdir)
        .output()
        .expect("failed to spawn grader")
}

#[test]
fn grader_pattern_uses_captured_variable() {
    // Pin the variable-capture pattern so a future "simplification" back to
    // an in-loop pipeline can't silently regress.
    for scenario in ["independent-parallel-n", "contention"] {
        let body = fs::read_to_string(grader(scenario)).unwrap();
        assert!(
            body.contains("log_out=$(git log"),
            "{scenario}/grade.sh lost log_out capture"
        );
        assert!(
            body.contains("printf '%s' \"$log_out\" | grep -qF"),
            "{scenario}/grade.sh lost printf|grep pattern"
        );
    }
}

#[test]
fn independent_parallel_grader_passes_with_one_matching_commit_each() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    write_doc(dir, "docs/topics/api-retry.md", "API retry");
    write_doc(dir, "docs/topics/db-config.md", "Database configuration");
    write_doc(dir, "docs/topics/graphql-errors.md", "GraphQL errors");
    write_doc(dir, "docs/topics/test-patterns.md", "Test patterns");
    git(dir, &["commit", "-q", "-m", "wip"]);

    for msg in [
        "docs: add api-retry topic page",
        "docs: add db-config topic page",
        "docs: add graphql-errors topic page",
        "docs: add test-patterns topic page",
    ] {
        commit_empty(dir, msg);
    }

    let out = run_grader(&grader("independent-parallel-n"), dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "expected pass, stderr: {stderr}");
    assert!(stderr.contains("\"passed\":true"), "stderr: {stderr}");
}

#[test]
fn independent_parallel_grader_passes_with_many_duplicate_matching_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    write_doc(dir, "docs/topics/api-retry.md", "API retry");
    write_doc(dir, "docs/topics/db-config.md", "Database configuration");
    write_doc(dir, "docs/topics/graphql-errors.md", "GraphQL errors");
    write_doc(dir, "docs/topics/test-patterns.md", "Test patterns");
    git(dir, &["commit", "-q", "-m", "wip"]);

    // Each commit message appears 5 times — exercises the "many matches" branch.
    for _ in 0..5 {
        for msg in [
            "docs: add api-retry topic page",
            "docs: add db-config topic page",
            "docs: add graphql-errors topic page",
            "docs: add test-patterns topic page",
        ] {
            commit_empty(dir, msg);
        }
    }

    let out = run_grader(&grader("independent-parallel-n"), dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "expected pass, stderr: {stderr}");
    assert!(stderr.contains("\"passed\":true"), "stderr: {stderr}");
}

#[test]
fn independent_parallel_grader_fails_with_zero_matching_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    write_doc(dir, "docs/topics/api-retry.md", "API retry");
    write_doc(dir, "docs/topics/db-config.md", "Database configuration");
    write_doc(dir, "docs/topics/graphql-errors.md", "GraphQL errors");
    write_doc(dir, "docs/topics/test-patterns.md", "Test patterns");
    git(dir, &["commit", "-q", "-m", "wip"]);

    // Only an unrelated commit — none of the four expected messages exist.
    commit_empty(dir, "chore: noise");

    let out = run_grader(&grader("independent-parallel-n"), dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected fail, got success. stderr: {stderr}"
    );
    assert!(
        stderr.contains("\"passed\":false"),
        "expected JSON failure body, got: {stderr}"
    );
    assert!(
        stderr.contains("git log missing commit"),
        "expected git-log failure detail, got: {stderr}"
    );
}

#[test]
fn contention_grader_passes_with_both_bumps_and_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\
         [dependencies]\nserde = \"1.0.220\"\ntokio = \"1.40\"\n",
    )
    .unwrap();
    git(dir, &["add", "Cargo.toml"]);
    git(dir, &["commit", "-q", "-m", "wip"]);

    commit_empty(dir, "deps: bump serde to 1.0.220");
    commit_empty(dir, "deps: bump tokio to 1.40");

    let out = run_grader(&grader("contention"), dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "expected pass, stderr: {stderr}");
    assert!(stderr.contains("\"passed\":true"), "stderr: {stderr}");
}

#[test]
fn contention_grader_fails_with_zero_matching_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\
         [dependencies]\nserde = \"1.0.220\"\ntokio = \"1.40\"\n",
    )
    .unwrap();
    git(dir, &["add", "Cargo.toml"]);
    commit_empty(dir, "chore: setup");

    let out = run_grader(&grader("contention"), dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected fail, got success. stderr: {stderr}"
    );
    assert!(stderr.contains("git log missing"), "stderr: {stderr}");
}
