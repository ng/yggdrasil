#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    collections::BTreeMap,
    path::Path,
    process::{Command, Output},
};
use uuid::Uuid;
use ygg::knowledge::{
    identity::{GitIdentity, IdentityRegistry},
    legacy::Mappings,
    runtime::{Binding, Phase, SELECTION_FILE},
    store::KnowledgeStore,
};

fn git(path: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    );
}
fn command(root: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ygg"));
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("YGG_CONFIG_DIR", root.join("config"))
        .env("YGG_DATA_DIR", root.join("data"))
        .env("YGG_KNOWLEDGE_DIR", root.join("bundle"))
        .env("YGG_KNOWLEDGE_POLICY_DIR", root.join("policy"))
        .env("YGG_USER", "legacy-user")
        .env("YGG_AGENT_NAME", "fixture-agent")
        // Deliberately invalid database configuration must not prevent local knowledge.
        .env("YGG_DB_MODE", "external")
        .current_dir(cwd)
        .arg("remember");
    cmd
}
fn json(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn fixture(root: &Path) -> (Binding, Uuid, Uuid) {
    std::fs::create_dir(root.join("repo")).unwrap();
    git(&root.join("repo"), &["init", "-q"]);
    git(
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
    git(
        &root.join("repo"),
        &[
            "worktree",
            "add",
            "--detach",
            root.join("worktree").to_str().unwrap(),
        ],
    );
    KnowledgeStore::open(&root.join("bundle"), true).unwrap();
    let registry = IdentityRegistry::open(&root.join("policy"), true).unwrap();
    registry.initialize(true).unwrap();
    let repo = registry
        .bind(&GitIdentity::discover(&root.join("repo")).unwrap())
        .unwrap();
    let (mut identities, revision) = registry.read().unwrap();
    let database = Uuid::new_v4();
    let legacy_repo = Uuid::new_v4();
    identities.repos[0]
        .databases
        .insert(database, [legacy_repo].into());
    registry.replace(&revision, &identities).unwrap();
    let agent = Uuid::new_v4();
    let binding = Binding {
        version: 1,
        minimum_client: 1,
        generation: 2,
        phase: Phase::Okf,
        bundle: root.join("bundle").canonicalize().unwrap(),
        mappings: Mappings {
            database_id: database,
            corpus_id: identities.corpus_id,
            repos: BTreeMap::from([(legacy_repo, repo)]),
            users: BTreeMap::from([("legacy-user".into(), "portable-user".into())]),
        },
        agents: BTreeMap::from([("fixture-agent".into(), agent)]),
    };
    (binding, legacy_repo, agent)
}
fn select(root: &Path, binding: &Binding) {
    use std::os::unix::fs::PermissionsExt;
    let file = root.join("policy").join(SELECTION_FILE);
    std::fs::write(&file, serde_json::to_vec(binding).unwrap()).unwrap();
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
}
#[test]
fn ordinary_remember_uses_selected_okf_offline_and_shares_worktree_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, repo, agent) = fixture(root);
    select(root, &binding);
    let note = json(
        command(root, &root.join("repo"))
            .args([" exact note Ω ", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(note["repo_id"], repo.to_string());
    assert_eq!(note["created_by"], agent.to_string());
    assert_eq!(note["text"], "exact note Ω");
    assert_eq!(note.as_object().unwrap().len(), 5);
    let global = json(
        command(root, root)
            .args(["global", "--global", "--json"])
            .output()
            .unwrap(),
    );
    assert!(global["repo_id"].is_null());
    let list = json(
        command(root, &root.join("worktree"))
            .args(["--list", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(list["count"], 2);
    assert_eq!(list["results"][1], note);
    assert!(
        !root.join("data").exists(),
        "knowledge access created managed/database state"
    );
    let failed = command(root, root)
        .arg("must not become global")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let list = json(
        command(root, root)
            .args(["--list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(list["count"], 2);
}
#[test]
fn fenced_invalid_or_unmapped_selection_never_falls_back_or_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = fixture(root);
    for fault in 0..4 {
        binding.phase = if fault == 0 {
            Phase::Fenced
        } else {
            Phase::Okf
        };
        binding.minimum_client = if fault == 1 { 999 } else { 1 };
        select(root, &binding);
        let mut cmd = command(root, &root.join("repo"));
        if fault == 2 {
            cmd.env("YGG_USER", "unmapped");
        }
        if fault == 3 {
            std::fs::write(root.join("policy").join(SELECTION_FILE), "broken").unwrap();
        }
        let output = cmd.args(["must not write", "--json"]).output().unwrap();
        assert!(!output.status.success());
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("requires DATABASE_URL"),
            "fell back to SQL"
        );
        assert!(
            KnowledgeStore::open(&root.join("bundle"), false)
                .unwrap()
                .snapshot()
                .documents
                .is_empty()
        );
        assert!(!root.join("data").exists());
    }
}
