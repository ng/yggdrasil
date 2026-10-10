#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};
use ygg::knowledge::{runtime::Binding, shared::Config};
fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
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
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn control(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn hook(root: &Path, session: &str) -> String {
    let mut child = okf::app(root, &root.join("repo"))
        .args(["hook", "pre-tool-use"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::to_string(&serde_json::json!({
                "tool_name":"Edit", "session_id": session, "tool_input":{"file_path":"src/x.rs"}
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
#[test]
fn ordinary_commands_publish_shared_rules_and_remote_revocation_and_outage_gate_injection() {
    let parent = tempfile::tempdir().unwrap();
    let remote = parent.path().join("remote.git");
    git(parent.path(), &["init", "--bare", remote.to_str().unwrap()]);
    let seed = parent.path().join("seed");
    git(
        parent.path(),
        &["init", "-b", "knowledge", seed.to_str().unwrap()],
    );
    git(&seed, &["commit", "--allow-empty", "-m", "initial"]);
    git(
        &seed,
        &[
            "push",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/knowledge",
        ],
    );
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let (binding_a, _, _) = okf::fixture(a.path());
    okf::fixture(b.path());
    let mut binding_b: Binding =
        serde_json::from_slice(&serde_json::to_vec(&binding_a).unwrap()).unwrap();
    binding_b.bundle = b.path().join("bundle").canonicalize().unwrap();
    control(
        &b.path().join("policy/identity.json"),
        &std::fs::read(a.path().join("policy/identity.json")).unwrap(),
    );
    let config = Config {
        version: 1,
        remote: remote.to_str().unwrap().into(),
        branch: "knowledge".into(),
    };
    okf::select(a.path(), &binding_a);
    okf::json(
        okf::command(a.path(), &a.path().join("repo"))
            .args(["PRIVATE_MUST_NOT_UPLOAD", "--global", "--json"])
            .output()
            .unwrap(),
    );
    for (root, binding) in [(a.path(), &binding_a), (b.path(), &binding_b)] {
        okf::select(root, binding);
        control(
            &root.join("policy/shared.json"),
            &serde_json::to_vec(&config).unwrap(),
        );
    }
    let note = okf::json(
        okf::command(a.path(), &a.path().join("repo"))
            .args(["shared note", "--global", "--json"])
            .output()
            .unwrap(),
    );
    let listed = okf::json(
        okf::command(b.path(), &b.path().join("repo"))
            .args(["--list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["results"][0], note);
    let rule = okf::json(
        okf::learn(a.path())
            .args([
                "create",
                "REMOTE_RULE",
                "--global",
                "--file-glob",
                "src/*.rs",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    okf::success(
        okf::app(b.path(), &b.path().join("repo"))
            .args(["knowledge", "sync"])
            .output()
            .unwrap(),
    );
    assert!(hook(b.path(), "first").contains("REMOTE_RULE"));
    let proposal = okf::json(
        okf::learn(a.path())
            .args([
                "propose",
                "PENDING_SHARED",
                "--global",
                "--file-glob",
                "src/*.rs",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert!(!hook(b.path(), "pending").contains("PENDING_SHARED"));
    okf::success(
        okf::learn(a.path())
            .env_remove("YGG_AGENT_NAME")
            .args(["approve", proposal["learning_id"].as_str().unwrap()])
            .output()
            .unwrap(),
    );
    okf::success(
        okf::app(b.path(), &b.path().join("repo"))
            .args(["knowledge", "sync"])
            .output()
            .unwrap(),
    );
    assert!(hook(b.path(), "approved").contains("PENDING_SHARED"));
    // A remote edit with old approval evidence is never an active instruction.
    git(&seed, &["fetch", remote.to_str().unwrap(), "knowledge"]);
    git(&seed, &["reset", "--hard", "FETCH_HEAD"]);
    let edited = seed.join(format!(
        "global/learnings/{}.md",
        proposal["learning_id"].as_str().unwrap()
    ));
    std::fs::write(
        &edited,
        std::fs::read_to_string(&edited)
            .unwrap()
            .replace("PENDING_SHARED", "EDITED_SHARED"),
    )
    .unwrap();
    git(&seed, &["add", "global"]);
    git(&seed, &["commit", "-m", "external content edit"]);
    git(
        &seed,
        &[
            "push",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/knowledge",
        ],
    );
    okf::success(
        okf::app(b.path(), &b.path().join("repo"))
            .args(["knowledge", "sync"])
            .output()
            .unwrap(),
    );
    assert!(!hook(b.path(), "edited-unapproved").contains("EDITED_SHARED"));
    okf::success(
        okf::learn(b.path())
            .env_remove("YGG_AGENT_NAME")
            .args(["approve", proposal["learning_id"].as_str().unwrap()])
            .output()
            .unwrap(),
    );
    okf::success(
        okf::app(a.path(), &a.path().join("repo"))
            .args(["knowledge", "sync"])
            .output()
            .unwrap(),
    );
    assert!(hook(a.path(), "reapproved").contains("EDITED_SHARED"));
    okf::success(
        okf::learn(a.path())
            .args(["delete", rule["learning_id"].as_str().unwrap()])
            .output()
            .unwrap(),
    );
    // A new session/explicit prime must refresh before automatic instructions.
    okf::success(
        okf::app(b.path(), &b.path().join("repo"))
            .arg("prime")
            .output()
            .unwrap(),
    );
    assert!(!hook(b.path(), "after-revocation").contains("REMOTE_RULE"));
    okf::json(
        okf::learn(a.path())
            .args([
                "create",
                "CACHED_RULE",
                "--global",
                "--file-glob",
                "src/*.rs",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    okf::success(
        okf::app(b.path(), &b.path().join("repo"))
            .args(["knowledge", "sync"])
            .output()
            .unwrap(),
    );
    assert!(hook(b.path(), "before-outage").contains("CACHED_RULE"));
    // A shared backup freezes Git state and excludes disposable materialized
    // views; its restored objects still support offline browsing.
    let cache = b.path().join("bundle");
    let abandoned = cache.join(format!(".view-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&abandoned).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", abandoned.join("ignored")).unwrap();
    let abandoned_init = cache.join(format!(".init-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&abandoned_init).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", abandoned_init.join("ignored")).unwrap();
    let archive = parent.path().join("archive");
    let manifest = ygg::knowledge::store::KnowledgeStore::open(&cache, false)
        .unwrap()
        .backup(&archive)
        .unwrap();
    assert!(
        !manifest
            .entries
            .keys()
            .any(|p| p.starts_with(".view-") || p.starts_with(".init-") || p == ".shared.lock")
    );
    let restored = parent.path().join("restored");
    ygg::knowledge::store::KnowledgeBackup::restore(&archive, &restored).unwrap();
    let saved = ygg::knowledge::shared::SharedGit::open(&restored, config.clone())
        .unwrap()
        .cached()
        .unwrap();
    assert!(
        saved
            .files
            .values()
            .any(|bytes| String::from_utf8_lossy(bytes).contains("CACHED_RULE"))
    );
    assert!(
        !saved
            .files
            .values()
            .any(|bytes| String::from_utf8_lossy(bytes).contains("PRIVATE_MUST_NOT_UPLOAD"))
    );
    let hidden = parent.path().join("offline.git");
    std::fs::rename(&remote, &hidden).unwrap();
    assert!(hook(b.path(), "fresh-cache-outage").contains("CACHED_RULE"));
    let prime = okf::app(b.path(), &b.path().join("repo"))
        .arg("prime")
        .output()
        .unwrap();
    assert!(prime.status.success());
    assert!(!String::from_utf8_lossy(&prime.stdout).contains("shared note"));
    assert!(!hook(b.path(), "after-failed-session-refresh").contains("CACHED_RULE"));
    let cached = okf::json(
        okf::learn(b.path())
            .args(["list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert!(
        cached["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["text"] == "CACHED_RULE")
    );
    let state_path = b.path().join("bundle/snapshot.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    state["confirmed_at"] = serde_json::json!(chrono::Utc::now() - chrono::Duration::seconds(61));
    control(&state_path, &serde_json::to_vec(&state).unwrap());
    assert!(!hook(b.path(), "stale-outage").contains("CACHED_RULE"));
    let failed = okf::learn(b.path())
        .args(["create", "MUST_NOT_PUBLISH", "--global", "--json"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    std::fs::rename(&hidden, &remote).unwrap();
    let sync = okf::json(
        okf::app(b.path(), &b.path().join("repo"))
            .args(["knowledge", "sync", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(sync["confirmed"], true);
    let list = okf::json(
        okf::learn(a.path())
            .args(["list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert!(
        !list["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["text"] == "MUST_NOT_PUBLISH")
    );
    // A real CLI failure after preparing a commit must remain inspectable even
    // without its optional read cache. The fixture Git wrapper fails only pushes.
    let bin = parent.path().join("fault-bin");
    std::fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("git");
    std::fs::write(&wrapper, "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = push ]; then exit 70; fi\ndone\nexec /usr/bin/git \"$@\"\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let fault_path = format!("{}:/usr/bin:/bin", bin.display());
    let failed = okf::command(a.path(), &a.path().join("repo"))
        .env("PATH", &fault_path)
        .args(["RECOVERED_NOTE", "--global", "--json"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    std::fs::remove_file(a.path().join("bundle/snapshot.json")).unwrap();
    let pending = okf::json(
        okf::app(a.path(), &a.path().join("repo"))
            .args(["knowledge", "pending", "--json"])
            .output()
            .unwrap(),
    );
    let commit = pending["commit"].as_str().unwrap();
    assert_eq!(pending["changes"].as_array().unwrap().len(), 1);
    for flags in [vec!["--json"], vec!["--retry", "--discard", "--json"]] {
        let invalid = okf::app(a.path(), &a.path().join("repo"))
            .args(["knowledge", "recover", commit])
            .args(flags)
            .output()
            .unwrap();
        assert!(!invalid.status.success());
    }
    let recovered = okf::json(
        okf::app(a.path(), &a.path().join("repo"))
            .args(["knowledge", "recover", commit, "--retry", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(recovered["outcome"], "published");
    let notes = okf::json(
        okf::command(b.path(), &b.path().join("repo"))
            .args(["--list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        notes["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["text"] == "RECOVERED_NOTE")
            .count(),
        1
    );
    let failed = okf::command(a.path(), &a.path().join("repo"))
        .env("PATH", &fault_path)
        .args(["ARCHIVED_NOTE", "--global", "--json"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let pending = okf::json(
        okf::app(a.path(), &a.path().join("repo"))
            .args(["knowledge", "pending", "--json"])
            .output()
            .unwrap(),
    );
    let discarded = okf::json(
        okf::app(a.path(), &a.path().join("repo"))
            .args([
                "knowledge",
                "recover",
                pending["commit"].as_str().unwrap(),
                "--discard",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(discarded["outcome"], "archived_unconfirmed");
    assert!(
        discarded["archive_ref"]
            .as_str()
            .unwrap()
            .starts_with("refs/ygg/drafts/")
    );
    let notes = okf::json(
        okf::command(b.path(), &b.path().join("repo"))
            .args(["--list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert!(
        !notes["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["text"] == "ARCHIVED_NOTE")
    );
    let pending = okf::json(
        okf::app(a.path(), &a.path().join("repo"))
            .args(["knowledge", "pending", "--json"])
            .output()
            .unwrap(),
    );
    assert!(pending.is_null());
    assert!(!a.path().join("data").exists() && !b.path().join("data").exists());
}
