#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    path::Path,
    process::Command,
    sync::{Arc, Barrier},
};
use uuid::Uuid;
use ygg::knowledge::identity::{GitIdentity, IdentityRegistry, canonical_url};

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-b", "main"]);
    git(
        path,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
}
fn registry(root: &Path) -> IdentityRegistry {
    let registry = IdentityRegistry::open(&root.join("policy"), true).unwrap();
    registry.initialize(true).unwrap();
    registry
}

#[test]
fn worktrees_and_clones_share_portable_ids_but_same_basename_does_not() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("one/repo");
    let b = temp.path().join("two/repo");
    repo(&a);
    repo(&b);
    let registry = registry(temp.path());
    let a_id = registry.bind(&GitIdentity::discover(&a).unwrap()).unwrap();
    let b_id = registry.bind(&GitIdentity::discover(&b).unwrap()).unwrap();
    assert_ne!(a_id, b_id);
    let worktree = temp.path().join("worktree");
    git(
        &a,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            worktree.to_str().unwrap(),
        ],
    );
    assert_eq!(
        registry
            .bind(&GitIdentity::discover(&worktree).unwrap())
            .unwrap(),
        a_id
    );
    git(
        &a,
        &["remote", "add", "origin", "git@github.com:Org/Project.git"],
    );
    assert_eq!(
        registry.bind(&GitIdentity::discover(&a).unwrap()).unwrap(),
        a_id
    );
    let clone = temp.path().join("clone");
    repo(&clone);
    git(
        &clone,
        &["remote", "add", "origin", "https://github.com/org/project"],
    );
    assert_eq!(
        registry
            .bind(&GitIdentity::discover(&clone).unwrap())
            .unwrap(),
        a_id
    );
    drop(registry);
    let reopened = IdentityRegistry::open(&temp.path().join("policy"), false).unwrap();
    assert_eq!(
        reopened
            .resolve(&GitIdentity::discover(&worktree).unwrap())
            .unwrap(),
        Some(a_id)
    );
    assert!(reopened.read().unwrap().0.trusted);
}

#[test]
fn changed_origin_requires_an_explicit_conditional_alias_edit() {
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("checkout");
    repo(&checkout);
    git(
        &checkout,
        &["remote", "add", "origin", "https://github.com/org/old"],
    );
    let registry = registry(temp.path());
    let id = registry
        .bind(&GitIdentity::discover(&checkout).unwrap())
        .unwrap();
    git(
        &checkout,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/org/renamed",
        ],
    );
    let current = GitIdentity::discover(&checkout).unwrap();
    assert!(registry.resolve(&current).is_err());
    assert!(registry.bind(&current).is_err());
    let (mut identities, revision) = registry.read().unwrap();
    identities
        .repos
        .iter_mut()
        .find(|r| r.id == id)
        .unwrap()
        .aliases
        .insert(current.origin.clone().unwrap());
    registry.replace(&revision, &identities).unwrap();
    assert_eq!(registry.bind(&current).unwrap(), id);
    assert!(registry.replace(&revision, &identities).is_err());
}

#[test]
fn ambiguous_scope_or_missing_database_mapping_never_becomes_global() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    repo(&a);
    repo(&b);
    git(&a, &["remote", "add", "origin", "https://github.com/org/a"]);
    git(&b, &["remote", "add", "origin", "https://github.com/org/b"]);
    let registry = registry(temp.path());
    let aid = registry.bind(&GitIdentity::discover(&a).unwrap()).unwrap();
    registry.bind(&GitIdentity::discover(&b).unwrap()).unwrap();
    git(
        &a,
        &["remote", "set-url", "origin", "https://github.com/org/b"],
    );
    assert!(registry.bind(&GitIdentity::discover(&a).unwrap()).is_err());
    let database = Uuid::new_v4();
    let legacy_repo = Uuid::new_v4();
    assert!(registry.from_legacy(database, legacy_repo).is_err());
    let (mut identities, revision) = registry.read().unwrap();
    identities
        .repos
        .iter_mut()
        .find(|r| r.id == aid)
        .unwrap()
        .databases
        .insert(database, [legacy_repo].into_iter().collect());
    registry.replace(&revision, &identities).unwrap();
    assert_eq!(registry.from_legacy(database, legacy_repo).unwrap(), aid);
    let (mut identities, revision) = registry.read().unwrap();
    identities
        .repos
        .iter_mut()
        .find(|r| r.id != aid)
        .unwrap()
        .databases
        .insert(database, [legacy_repo].into_iter().collect());
    assert!(registry.replace(&revision, &identities).is_err());
    assert!(GitIdentity::discover(temp.path()).is_err());
}

#[test]
fn aliases_do_not_store_secrets_or_collapse_distinct_custom_servers() {
    let cwd = Path::new("/");
    assert_eq!(
        canonical_url("https://user:password@GitHub.com/Org/Repo.git/", cwd).unwrap(),
        "https://github.com/org/repo"
    );
    assert_eq!(
        canonical_url("git@github.com:org/repo.git", cwd).unwrap(),
        "https://github.com/org/repo"
    );
    assert_ne!(
        canonical_url("ssh://alice@host/repo", cwd).unwrap(),
        canonical_url("ssh://bob@host/repo", cwd).unwrap()
    );
    assert_ne!(
        canonical_url("alice@host:repo", cwd).unwrap(),
        canonical_url("ssh://alice@host/repo", cwd).unwrap()
    );
    assert_ne!(
        canonical_url("ssh://git@github.com:2222/org/repo", cwd).unwrap(),
        canonical_url("git@github.com:org/repo", cwd).unwrap()
    );
    assert_ne!(
        canonical_url("https://host/repo.git", cwd).unwrap(),
        canonical_url("https://host/repo", cwd).unwrap()
    );
    let error = canonical_url("https://user:secret@host/git?repo=a", cwd).unwrap_err();
    assert!(!error.to_string().contains("secret"));
    assert!(canonical_url("custom://host/repo", cwd).is_err());
}

#[test]
fn concurrent_binding_creates_one_identity_and_never_changes_trust_on_open() {
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("repo");
    repo(&checkout);
    let registry = registry(temp.path());
    let corpus = registry.read().unwrap().0.corpus_id;
    let barrier = Arc::new(Barrier::new(12));
    let mut workers = Vec::new();
    for _ in 0..12 {
        let root = temp.path().join("policy");
        let checkout = checkout.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let registry = IdentityRegistry::open(&root, false).unwrap();
            let git = GitIdentity::discover(&checkout).unwrap();
            barrier.wait();
            registry.bind(&git).unwrap()
        }));
    }
    let ids: std::collections::BTreeSet<_> =
        workers.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(ids.len(), 1);
    assert_eq!(registry.read().unwrap().0.repos.len(), 1);
    assert_eq!(registry.initialize(true).unwrap().corpus_id, corpus);
    assert!(registry.initialize(false).is_err());
    let (mut identities, revision) = registry.read().unwrap();
    identities.corpus_id = Uuid::new_v4();
    assert!(registry.replace(&revision, &identities).is_err());
}

#[test]
fn backup_and_explicit_database_rebinding_preserve_corpus_and_repository_ids() {
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("checkout");
    repo(&checkout);
    let source = registry(temp.path());
    let id = source
        .bind(&GitIdentity::discover(&checkout).unwrap())
        .unwrap();
    let old_database = Uuid::new_v4();
    let new_database = Uuid::new_v4();
    let first_legacy = Uuid::new_v4();
    let duplicate_legacy = Uuid::new_v4();
    let (mut config, revision) = source.read().unwrap();
    config.repos[0].databases.insert(
        old_database,
        [first_legacy, duplicate_legacy].into_iter().collect(),
    );
    source.replace(&revision, &config).unwrap();
    assert_eq!(
        source.from_legacy(old_database, duplicate_legacy).unwrap(),
        id
    );
    let restored_dir = temp.path().join("restored-policy");
    let restored = IdentityRegistry::open(&restored_dir, true).unwrap();
    std::fs::copy(
        temp.path().join("policy/identity.json"),
        restored_dir.join("identity.json"),
    )
    .unwrap();
    let (mut config, revision) = restored.read().unwrap();
    let corpus = config.corpus_id;
    config.repos[0].databases.insert(
        new_database,
        [first_legacy, duplicate_legacy].into_iter().collect(),
    );
    restored.replace(&revision, &config).unwrap();
    assert_eq!(
        restored.from_legacy(new_database, first_legacy).unwrap(),
        id
    );
    assert_eq!(restored.read().unwrap().0.corpus_id, corpus);
    assert!(source.from_legacy(new_database, first_legacy).is_err());
}

#[test]
fn corrupted_or_symlinked_configuration_never_regenerates_identity() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let registry = registry(temp.path());
    let config = temp.path().join("policy/identity.json");
    std::fs::write(&config, "broken configuration").unwrap();
    assert!(registry.read().is_err());
    assert!(registry.initialize(true).is_err());
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "broken configuration"
    );
    std::fs::remove_file(&config).unwrap();
    let target = temp.path().join("external");
    std::fs::write(&target, "must not touch").unwrap();
    symlink(&target, &config).unwrap();
    assert!(registry.read().is_err());
    assert!(registry.initialize(true).is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "must not touch");
}

#[test]
fn discovery_preserves_whitespace_in_git_directory_names() {
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("repo \n");
    repo(&checkout);
    let git = GitIdentity::discover(&checkout).unwrap();
    assert_eq!(
        git.common_dir,
        checkout.join(".git").canonicalize().unwrap()
    );
    assert!(git.origin.is_none());
}
