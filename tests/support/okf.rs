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

pub fn git(path: &Path, args: &[&str]) {
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
pub fn app(root: &Path, cwd: &Path) -> Command {
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
        .current_dir(cwd);
    cmd
}
pub fn command(root: &Path, cwd: &Path) -> Command {
    let mut cmd = app(root, cwd);
    cmd.arg("remember");
    cmd
}
pub fn learn(root: &Path) -> Command {
    let mut cmd = app(root, &root.join("repo"));
    cmd.arg("learn");
    cmd
}
pub fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
pub fn json(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
pub fn fixture(root: &Path) -> (Binding, Uuid, Uuid) {
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
pub fn select(root: &Path, binding: &Binding) {
    use std::os::unix::fs::PermissionsExt;
    let file = root.join("policy").join(SELECTION_FILE);
    std::fs::write(&file, serde_json::to_vec(binding).unwrap()).unwrap();
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
}
