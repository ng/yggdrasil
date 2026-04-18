//! Worktree helper — provisions git worktrees per task so the click-to-do
//! supervisor can spawn CC sessions in isolated working copies.
//!
//! Layout:
//!   root = `$XDG_STATE_HOME/ygg/worktrees` (falls back to
//!          `$HOME/.local/state/ygg/worktrees`)
//!   path = <root>/<repo-prefix>-<seq>
//!   branch = ygg/<repo-prefix>-<seq>
//!
//! Idempotent: `ensure` returns the existing worktree if the branch is
//! already checked out at our path. `teardown` prunes the worktree and
//! optionally deletes the branch per policy.

use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

use crate::models::repo::{Repo, RepoRepo};
use crate::models::task::{Task, TaskRepo};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TeardownPolicy {
    /// Leave the worktree + branch alone. Default for inspection after close.
    Keep,
    /// Prune the worktree but leave the branch for potential PR.
    Archive,
    /// Prune the worktree and delete the local branch.
    Delete,
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub task_ref: String,   // "yggdrasil-42"
    pub branch: String,     // "ygg/yggdrasil-42"
    pub path: PathBuf,
    pub base_path: PathBuf, // repo root we worktreed from
}

/// Create (or return existing) worktree for the given task. The caller is
/// responsible for having at least one local_path recorded on the repo.
pub async fn ensure(
    pool: &sqlx::PgPool,
    task_id: Uuid,
) -> Result<Worktree, anyhow::Error> {
    let (task, repo) = resolve(pool, task_id).await?;
    let task_ref = format!("{}-{}", repo.task_prefix, task.seq);
    let branch = format!("ygg/{task_ref}");
    let root = worktree_root()?;
    let path = root.join(&task_ref);

    let base = primary_local_path(&repo)
        .ok_or_else(|| anyhow::anyhow!(
            "repo '{}' has no local_paths recorded — can't create worktree",
            repo.name
        ))?;

    // Fast path: worktree already exists at our path (idempotent).
    if path.exists() && is_worktree_of(&base, &path) {
        return Ok(Worktree {
            task_ref,
            branch,
            path,
            base_path: base.clone(),
        });
    }

    std::fs::create_dir_all(&root)
        .map_err(|e| anyhow::anyhow!("create worktree root {}: {e}", root.display()))?;

    // Pick a base commit: current HEAD of the source repo. Resolve once so
    // we don't race with the source branch moving between detection and
    // `git worktree add`.
    let base_commit = git_stdout(&base, &["rev-parse", "HEAD"])?;

    // If the branch already exists (leftover from a prior run), just check
    // it out; otherwise create it.
    let branch_exists = git(&base, &["show-ref", "--verify", "--quiet",
                                     &format!("refs/heads/{branch}")]).is_ok();
    let args: Vec<String> = if branch_exists {
        vec!["worktree".into(), "add".into(),
             path.to_string_lossy().into(), branch.clone()]
    } else {
        vec!["worktree".into(), "add".into(), "-b".into(), branch.clone(),
             path.to_string_lossy().into(), base_commit]
    };
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    git(&base, &args_ref)?;

    Ok(Worktree { task_ref, branch, path, base_path: base })
}

/// Remove the worktree per policy. `Keep` is a no-op. `Delete` also deletes
/// the local branch; `Archive` leaves the branch. Never force-removes if
/// the worktree has uncommitted changes unless `force = true`.
pub async fn teardown(
    pool: &sqlx::PgPool,
    task_id: Uuid,
    policy: TeardownPolicy,
    force: bool,
) -> Result<(), anyhow::Error> {
    if policy == TeardownPolicy::Keep { return Ok(()); }

    let (task, repo) = resolve(pool, task_id).await?;
    let task_ref = format!("{}-{}", repo.task_prefix, task.seq);
    let branch = format!("ygg/{task_ref}");
    let path = worktree_root()?.join(&task_ref);
    let base = primary_local_path(&repo)
        .ok_or_else(|| anyhow::anyhow!("no local_paths for repo {}", repo.name))?;

    if !path.exists() {
        // Already gone — still try to prune stale registrations + branch.
        let _ = git(&base, &["worktree", "prune"]);
        if policy == TeardownPolicy::Delete {
            let _ = git(&base, &["branch", "-D", &branch]);
        }
        return Ok(());
    }

    let mut args: Vec<&str> = vec!["worktree", "remove"];
    let path_str = path.to_string_lossy().into_owned();
    if force { args.push("--force"); }
    args.push(&path_str);
    git(&base, &args)?;

    if policy == TeardownPolicy::Delete {
        // -D (force) because the branch may have work we already decided to drop.
        let _ = git(&base, &["branch", "-D", &branch]);
    }
    Ok(())
}

pub fn worktree_root() -> Result<PathBuf, anyhow::Error> {
    if let Ok(x) = std::env::var("XDG_STATE_HOME") {
        if !x.is_empty() {
            return Ok(PathBuf::from(x).join("ygg/worktrees"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME not set — cannot locate worktree root"))?;
    Ok(PathBuf::from(home).join(".local/state/ygg/worktrees"))
}

/// Lookup the task + its repo in one shot.
async fn resolve(pool: &sqlx::PgPool, task_id: Uuid) -> Result<(Task, Repo), anyhow::Error> {
    let task = TaskRepo::new(pool).get(task_id).await?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} not found"))?;
    let repo = RepoRepo::new(pool).get(task.repo_id).await?
        .ok_or_else(|| anyhow::anyhow!("repo {} not found", task.repo_id))?;
    Ok((task, repo))
}

/// Pick the first local_path that's an actual git working tree. Repos
/// register multiple candidate paths over time; some may be stale.
fn primary_local_path(repo: &Repo) -> Option<PathBuf> {
    for p in &repo.local_paths {
        let path = PathBuf::from(p);
        if path.join(".git").exists() || is_git_dir(&path) {
            return Some(path);
        }
    }
    // Fall back to the first path; may fail later with a clearer git error.
    repo.local_paths.first().map(PathBuf::from)
}

fn is_git_dir(p: &Path) -> bool {
    // Accept both working-tree repos (`.git` dir) and bare repos.
    p.join("HEAD").exists() && p.join("refs").exists()
}

/// Ask the source repo whether `path` is already one of its worktrees. Used
/// for idempotence — reuse rather than re-add.
fn is_worktree_of(base: &Path, path: &Path) -> bool {
    let Ok(list) = git_stdout(base, &["worktree", "list", "--porcelain"]) else { return false; };
    let needle = path.to_string_lossy().to_string();
    list.lines().any(|l| l.trim_start_matches("worktree ") == needle)
}

fn git(cwd: &Path, args: &[&str]) -> Result<(), anyhow::Error> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()
        .map_err(|e| anyhow::anyhow!("spawn git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String, anyhow::Error> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()
        .map_err(|e| anyhow::anyhow!("spawn git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn parse_policy(s: &str) -> Result<TeardownPolicy, anyhow::Error> {
    match s {
        "keep" => Ok(TeardownPolicy::Keep),
        "archive" => Ok(TeardownPolicy::Archive),
        "delete" => Ok(TeardownPolicy::Delete),
        other => anyhow::bail!("unknown teardown policy '{other}' — try keep/archive/delete"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_parse() {
        assert_eq!(parse_policy("keep").unwrap(), TeardownPolicy::Keep);
        assert_eq!(parse_policy("archive").unwrap(), TeardownPolicy::Archive);
        assert_eq!(parse_policy("delete").unwrap(), TeardownPolicy::Delete);
        assert!(parse_policy("nope").is_err());
    }

    #[test]
    fn root_env_override() {
        // Can't rely on std::env in parallel tests on 2024 edition; use
        // a pure-function path instead: HOME fallback.
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            let r = worktree_root().unwrap();
            assert!(r.ends_with("ygg/worktrees"));
        }
    }
}
