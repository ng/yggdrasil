//! Regression for `ygg integrate --global` opts (yggdrasil-172).

use ygg::cli::init_project::{
    IntegrateOpts, has_managed_block, install, install_with, remove, remove_with,
};

#[test]
fn default_install_writes_both_claude_and_agents() {
    let dir = tempfile::tempdir().unwrap();
    let report = install(dir.path()).unwrap();
    let names: Vec<String> = report
        .files
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "CLAUDE.md"));
    assert!(names.iter().any(|n| n == "AGENTS.md"));
}

#[test]
fn skip_agents_md_writes_only_claude_md() {
    let dir = tempfile::tempdir().unwrap();
    let opts = IntegrateOpts {
        skip_agents_md: true,
    };
    let report = install_with(dir.path(), opts).unwrap();
    let names: Vec<String> = report
        .files
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "CLAUDE.md"));
    assert!(
        !names.iter().any(|n| n == "AGENTS.md"),
        "AGENTS.md must be skipped with skip_agents_md=true"
    );
    assert!(!dir.path().join("AGENTS.md").exists());
}

#[test]
fn install_creates_parent_dirs_for_global_path() {
    // Mimics writing into a fresh ~/.claude that doesn't yet exist.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nested/deeper/.claude");
    install_with(
        &target,
        IntegrateOpts {
            skip_agents_md: true,
        },
    )
    .unwrap();
    assert!(target.join("CLAUDE.md").exists());
}

#[test]
fn has_managed_block_after_install() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!has_managed_block(dir.path()));
    install(dir.path()).unwrap();
    assert!(has_managed_block(dir.path()));
}

#[test]
fn remove_with_skip_agents_drops_only_claude_md() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path()).unwrap();
    // Both files exist now.
    assert!(dir.path().join("CLAUDE.md").exists());
    assert!(dir.path().join("AGENTS.md").exists());
    // Remove only CLAUDE.md (the --global path).
    remove_with(
        dir.path(),
        IntegrateOpts {
            skip_agents_md: true,
        },
    )
    .unwrap();
    assert!(!dir.path().join("CLAUDE.md").exists());
    assert!(
        dir.path().join("AGENTS.md").exists(),
        "AGENTS.md must survive a --global --remove"
    );
    // Default remove sweeps the rest.
    remove(dir.path()).unwrap();
    assert!(!dir.path().join("AGENTS.md").exists());
}

#[test]
fn install_idempotent_on_repeat_runs() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path()).unwrap();
    let report = install(dir.path()).unwrap();
    use ygg::cli::init_project::ActionTaken;
    assert!(
        report
            .files
            .iter()
            .all(|(_, a)| matches!(a, ActionTaken::Unchanged))
    );
}
