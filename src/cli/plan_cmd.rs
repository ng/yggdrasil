//! `ygg plan` — execute tasks with worktrees + CC sessions.
//!
//! Three subcommands that together form the click-to-do surface:
//!
//!   ygg plan create <name>                — shortcut to create an epic
//!   ygg plan add    <epic> <title> [--deps REF,REF] [--kind ...]
//!   ygg plan run    <task-ref> [--dry-run]
//!
//! `plan run` is the single-task synchronous path: ensure worktree →
//! spawn CC session in that worktree → claim task. Multi-task dep-driven
//! orchestration is the supervisor (yggdrasil-44).

use std::process::Command;
use uuid::Uuid;

use crate::models::repo::RepoRepo;
use crate::models::task::{Task, TaskKind, TaskRepo, TaskStatus};
use crate::worktree;

pub async fn create(
    pool: &sqlx::PgPool,
    title: &str,
    description: Option<&str>,
    agent_name: &str,
) -> Result<Task, anyhow::Error> {
    let repo = crate::cli::task_cmd::resolve_cwd_repo(pool).await?;
    let task_repo = TaskRepo::new(pool);
    let agent_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT agent_id FROM agents WHERE agent_name = $1"
    ).bind(agent_name).fetch_optional(pool).await?.flatten();

    let created = task_repo.create(repo.repo_id, agent_id, crate::models::task::TaskCreate {
        title,
        description: description.unwrap_or(""),
        acceptance: None,
        design: None,
        notes: None,
        kind: TaskKind::Epic,
        priority: 1,
        assignee: agent_id,
        labels: &[],
    }).await?;
    println!("Created plan {}-{}  {}", repo.task_prefix, created.seq, title);
    Ok(created)
}

pub async fn add(
    pool: &sqlx::PgPool,
    epic_ref: &str,
    title: &str,
    description: Option<&str>,
    kind: Option<&str>,
    deps: &[String],
    agent_name: &str,
) -> Result<Task, anyhow::Error> {
    let epic = crate::cli::task_cmd::resolve_task_public(pool, epic_ref).await?;
    let repo = RepoRepo::new(pool).get(epic.repo_id).await?
        .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;
    let agent_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT agent_id FROM agents WHERE agent_name = $1"
    ).bind(agent_name).fetch_optional(pool).await?.flatten();
    let task_kind = match kind {
        Some(k) => <TaskKind as std::str::FromStr>::from_str(k)
            .map_err(|e| anyhow::anyhow!(e))?,
        None => TaskKind::Task,
    };

    let task_repo = TaskRepo::new(pool);
    let child = task_repo.create(repo.repo_id, agent_id, crate::models::task::TaskCreate {
        title,
        description: description.unwrap_or(""),
        acceptance: None,
        design: None,
        notes: None,
        kind: task_kind,
        priority: 2,
        assignee: None,
        labels: &[],
    }).await?;

    // Wire the child under the epic and apply declared deps.
    task_repo.add_dep(epic.task_id, child.task_id).await?;
    for d in deps {
        let dep = crate::cli::task_cmd::resolve_task_public(pool, d).await?;
        task_repo.add_dep(child.task_id, dep.task_id).await?;
    }

    println!("Added {}-{}  under {epic_ref}  {}", repo.task_prefix, child.seq, title);
    if !deps.is_empty() {
        println!("  deps: {}", deps.join(", "));
    }
    Ok(child)
}

pub async fn run(
    pool: &sqlx::PgPool,
    task_ref: &str,
    agent_name: &str,
    dry_run: bool,
) -> Result<(), anyhow::Error> {
    let task = crate::cli::task_cmd::resolve_task_public(pool, task_ref).await?;
    let repo = RepoRepo::new(pool).get(task.repo_id).await?
        .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;

    // Refuse to run a task whose blockers aren't all closed — that's the
    // supervisor's job; single-shot run should be a leaf operation.
    let deps = TaskRepo::new(pool).deps(task.task_id).await?;
    let unresolved: Vec<_> = deps.iter()
        .filter(|d| d.status != TaskStatus::Closed)
        .collect();
    if !unresolved.is_empty() {
        println!("{}-{}  has {} open blocker(s) — refusing to run.",
            repo.task_prefix, task.seq, unresolved.len());
        for d in &unresolved {
            println!("  · {}-{}  [{}]  {}", repo.task_prefix, d.seq, d.status, d.title);
        }
        println!("Close the blockers first, or use `ygg plan supervise` when it ships.");
        return Ok(());
    }

    if dry_run {
        println!("DRY RUN:");
        println!("  1. ensure worktree for {}-{}", repo.task_prefix, task.seq);
        println!("  2. claim task (assignee = {agent_name}, status → in_progress)");
        println!("  3. tmux new-window 'ygg-{}-{}' in the worktree", repo.task_prefix, task.seq);
        println!("  4. launch `claude` in that window with a priming prompt");
        return Ok(());
    }

    // 1. Worktree
    let wt = worktree::ensure(pool, task.task_id).await?;
    println!("worktree: {}", wt.path.display());

    // 2. Claim the task
    let agent_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT agent_id FROM agents WHERE agent_name = $1"
    ).bind(agent_name).fetch_optional(pool).await?.flatten();
    if let Some(aid) = agent_id {
        let _ = TaskRepo::new(pool).set_status(
            task.task_id, TaskStatus::InProgress, Some(aid), None,
        ).await;
    }

    // 3. tmux window + 4. launch claude. Uses the same tmux session the
    // rest of ygg spawn/up use, window name encodes the task ref.
    let window = format!("run-{}-{}", repo.task_prefix, task.seq);
    spawn_tmux(&window, &wt.path, &task, &repo)?;

    println!("launched {} in tmux window '{window}'", task_ref_display(&repo.task_prefix, task.seq));
    println!("  attach: tmux attach -t yggdrasil");
    Ok(())
}

fn task_ref_display(prefix: &str, seq: i32) -> String {
    format!("{prefix}-{seq}")
}

fn spawn_tmux(
    window: &str,
    cwd: &std::path::Path,
    task: &Task,
    repo: &crate::models::repo::Repo,
) -> Result<(), anyhow::Error> {
    const SESSION: &str = "yggdrasil";
    // Ensure session exists; harmless if it does.
    let _ = Command::new("tmux")
        .args(["new-session", "-d", "-s", SESSION, "-n", "dashboard"])
        .status();
    // Create the window anchored to the worktree cwd.
    let status = Command::new("tmux")
        .args([
            "new-window", "-t", SESSION, "-n", window,
            "-c", &cwd.to_string_lossy(),
        ])
        .status()
        .map_err(|e| anyhow::anyhow!("tmux new-window: {e}"))?;
    if !status.success() {
        anyhow::bail!("tmux new-window failed for '{window}'");
    }

    // Ship the priming prompt to Claude Code via a one-shot file. Claude
    // reads it from stdin; cleaner than pasting through tmux send-keys
    // which can mangle long content.
    let prime = prime_prompt(task, repo);
    let target = format!("{SESSION}:{window}");
    Command::new("tmux")
        .args([
            "send-keys", "-t", &target,
            &format!("claude <<'YGG_EOF'\n{prime}\nYGG_EOF"),
            "Enter",
        ])
        .status()
        .map_err(|e| anyhow::anyhow!("tmux send-keys: {e}"))?;

    Ok(())
}

fn prime_prompt(task: &Task, repo: &crate::models::repo::Repo) -> String {
    let mut p = format!(
        "You have been spawned to work on a single ygg task.\n\n\
         **Task: {}-{}**  [{}]  P{}  kind={:?}\n\n\
         **Title:** {}\n\n",
        repo.task_prefix, task.seq, task.status, task.priority, task.kind,
        task.title,
    );
    if !task.description.is_empty() {
        p.push_str(&format!("**Description:**\n{}\n\n", task.description));
    }
    if let Some(a) = &task.acceptance {
        if !a.is_empty() { p.push_str(&format!("**Acceptance:**\n{a}\n\n")); }
    }
    if let Some(d) = &task.design {
        if !d.is_empty() { p.push_str(&format!("**Design:**\n{d}\n\n")); }
    }
    p.push_str(
        "Work in this git worktree. When complete, close the task with:\n  \
         `ygg task close "
    );
    p.push_str(&format!("{}-{} --reason \"...\"`\n", repo.task_prefix, task.seq));
    p
}
