use crate::models::agent::AgentRepo;
use crate::models::repo::{detect_git_repo, slugify, RepoRepo};
use crate::models::task::{Task, TaskCreate, TaskKind, TaskRepo, TaskStatus, TaskUpdate};
use std::str::FromStr;
use uuid::Uuid;

/// Resolve the current working directory to a repo row, registering it
/// if this is the first time we've seen it. Falls back to a
/// non-git placeholder keyed on the absolute path.
pub async fn resolve_cwd_repo(pool: &sqlx::PgPool) -> Result<crate::models::repo::Repo, anyhow::Error> {
    let cwd = std::env::current_dir()?;
    let repo_repo = RepoRepo::new(pool);

    if let Some((url, toplevel, name)) = detect_git_repo(&cwd) {
        let prefix = slugify(&name);
        let repo = repo_repo
            .register(url.as_deref(), &name, &prefix, Some(&toplevel))
            .await?;
        return Ok(repo);
    }

    // Non-git directory: key by absolute path, prefix from basename
    let path_str = cwd.to_string_lossy().to_string();
    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "scratch".to_string());
    let prefix = slugify(&name);
    let repo = repo_repo
        .register(None, &name, &prefix, Some(&path_str))
        .await?;
    Ok(repo)
}

async fn resolve_agent_id(
    pool: &sqlx::PgPool,
    agent_name: &str,
) -> Result<Option<Uuid>, anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    Ok(agent_repo.get_by_name(agent_name).await?.map(|a| a.agent_id))
}

/// Parse a task reference: either a UUID, or a "<prefix>-NNN" string.
async fn resolve_task(
    pool: &sqlx::PgPool,
    reference: &str,
) -> Result<Task, anyhow::Error> {
    if let Ok(uuid) = Uuid::parse_str(reference) {
        let t = TaskRepo::new(pool).get(uuid).await?
            .ok_or_else(|| anyhow::anyhow!("task {uuid} not found"))?;
        return Ok(t);
    }

    // <prefix>-<seq>
    let (prefix, seq_str) = reference.rsplit_once('-')
        .ok_or_else(|| anyhow::anyhow!("expected <prefix>-<seq> or UUID, got {reference}"))?;
    let seq: i32 = seq_str.parse()
        .map_err(|_| anyhow::anyhow!("sequence must be an integer: {seq_str}"))?;

    let repo = RepoRepo::new(pool).get_by_prefix(prefix).await?
        .ok_or_else(|| anyhow::anyhow!("no repo with prefix '{prefix}'"))?;
    let t = TaskRepo::new(pool).get_by_ref(repo.repo_id, seq).await?
        .ok_or_else(|| anyhow::anyhow!("task {prefix}-{seq} not found"))?;
    Ok(t)
}

pub struct CreateOpts<'a> {
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub priority: Option<i16>,
    pub acceptance: Option<&'a str>,
    pub design: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub labels: &'a [String],
    pub agent_name: &'a str,
}

pub async fn create(pool: &sqlx::PgPool, opts: CreateOpts<'_>) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let created_by = resolve_agent_id(pool, opts.agent_name).await?;

    let kind = opts.kind
        .map(|k| TaskKind::from_str(k).map_err(|e| anyhow::anyhow!(e)))
        .transpose()?
        .unwrap_or_default();

    let priority = opts.priority.unwrap_or(2);
    if !(0..=4).contains(&priority) {
        anyhow::bail!("priority must be between 0 (critical) and 4 (backlog)");
    }

    let task = TaskRepo::new(pool)
        .create(
            repo.repo_id,
            created_by,
            TaskCreate {
                title: opts.title,
                description: opts.description.unwrap_or(""),
                acceptance: opts.acceptance,
                design: opts.design,
                notes: opts.notes,
                kind,
                priority,
                assignee: None,
                labels: opts.labels,
            },
        )
        .await?;

    println!("Created {}-{}  {}", repo.task_prefix, task.seq, task.title);
    Ok(())
}

pub async fn list(
    pool: &sqlx::PgPool,
    all_repos: bool,
    status: Option<&str>,
) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let status = status
        .map(TaskStatus::from_str)
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?;

    let tasks = TaskRepo::new(pool).list(repo_id, status).await?;
    print_task_table(pool, &tasks).await
}

pub async fn ready(pool: &sqlx::PgPool) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let tasks = TaskRepo::new(pool).ready(repo.repo_id).await?;
    if tasks.is_empty() {
        println!("No ready tasks in {}.", repo.name);
        return Ok(());
    }
    print_task_table(pool, &tasks).await
}

pub async fn blocked(pool: &sqlx::PgPool) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let tasks = TaskRepo::new(pool).blocked(repo.repo_id).await?;
    if tasks.is_empty() {
        println!("No blocked tasks in {}.", repo.name);
        return Ok(());
    }
    print_task_table(pool, &tasks).await
}

pub async fn show(pool: &sqlx::PgPool, reference: &str) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let repo = RepoRepo::new(pool).get(t.repo_id).await?
        .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;
    let labels = TaskRepo::new(pool).labels(t.task_id).await?;
    let deps = TaskRepo::new(pool).deps(t.task_id).await?;

    println!("{}-{}  [{}]  P{}  {}", repo.task_prefix, t.seq, t.status, t.priority, t.kind);
    println!();
    println!("  {}", t.title);
    if !t.description.is_empty() {
        println!();
        for line in t.description.lines() {
            println!("  {line}");
        }
    }
    if let Some(a) = &t.acceptance {
        println!();
        println!("  Acceptance:");
        for line in a.lines() { println!("    {line}"); }
    }
    if let Some(d) = &t.design {
        println!();
        println!("  Design:");
        for line in d.lines() { println!("    {line}"); }
    }
    if let Some(n) = &t.notes {
        println!();
        println!("  Notes:");
        for line in n.lines() { println!("    {line}"); }
    }
    if !labels.is_empty() {
        println!();
        println!("  Labels: {}", labels.join(", "));
    }
    if !deps.is_empty() {
        println!();
        println!("  Depends on:");
        for d in &deps {
            let indicator = if matches!(d.status, TaskStatus::Closed) { "✓" } else { "·" };
            println!("    {indicator} {}-{} [{}] {}", repo.task_prefix, d.seq, d.status, d.title);
        }
    }
    if t.human_flag {
        println!();
        println!("  [flagged for human decision]");
    }
    Ok(())
}

pub async fn update(
    pool: &sqlx::PgPool,
    reference: &str,
    title: Option<&str>,
    description: Option<&str>,
    priority: Option<i16>,
    kind: Option<&str>,
    acceptance: Option<&str>,
    design: Option<&str>,
    notes: Option<&str>,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;

    let kind = kind
        .map(|k| TaskKind::from_str(k).map_err(|e| anyhow::anyhow!(e)))
        .transpose()?;

    TaskRepo::new(pool).update(t.task_id, agent_id, TaskUpdate {
        title, description, acceptance, design, notes, kind, priority,
        assignee: None, human_flag: None,
    }).await?;
    println!("Updated {}", reference);
    Ok(())
}

pub async fn set_status(
    pool: &sqlx::PgPool,
    reference: &str,
    status: &str,
    reason: Option<&str>,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    let status = TaskStatus::from_str(status).map_err(|e| anyhow::anyhow!(e))?;

    TaskRepo::new(pool).set_status(t.task_id, status.clone(), agent_id, reason).await?;
    println!("{reference} → {status}");
    Ok(())
}

pub async fn claim(
    pool: &sqlx::PgPool,
    reference: &str,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    TaskRepo::new(pool).update(t.task_id, agent_id, TaskUpdate {
        assignee: Some(agent_id),
        ..Default::default()
    }).await?;
    TaskRepo::new(pool).set_status(t.task_id, TaskStatus::InProgress, agent_id, None).await?;
    println!("{reference} claimed by {agent_name}");
    Ok(())
}

pub async fn close(
    pool: &sqlx::PgPool,
    reference: &str,
    reason: Option<&str>,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    set_status(pool, reference, "closed", reason, agent_name).await
}

pub async fn add_dep(pool: &sqlx::PgPool, task_ref: &str, blocker_ref: &str) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, task_ref).await?;
    let b = resolve_task(pool, blocker_ref).await?;
    TaskRepo::new(pool).add_dep(t.task_id, b.task_id).await?;
    println!("{task_ref} now depends on {blocker_ref}");
    Ok(())
}

pub async fn remove_dep(pool: &sqlx::PgPool, task_ref: &str, blocker_ref: &str) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, task_ref).await?;
    let b = resolve_task(pool, blocker_ref).await?;
    TaskRepo::new(pool).remove_dep(t.task_id, b.task_id).await?;
    println!("dependency removed: {task_ref} ← {blocker_ref}");
    Ok(())
}

pub async fn label(pool: &sqlx::PgPool, reference: &str, label: &str) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    TaskRepo::new(pool).add_label(t.task_id, label).await?;
    println!("{reference} + {label}");
    Ok(())
}

pub async fn stats(pool: &sqlx::PgPool, all_repos: bool) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let s = TaskRepo::new(pool).stats(repo_id).await?;
    println!(
        "open: {}    in_progress: {}    blocked: {}    closed: {}",
        s.open, s.in_progress, s.blocked, s.closed
    );
    Ok(())
}

async fn print_task_table(pool: &sqlx::PgPool, tasks: &[Task]) -> Result<(), anyhow::Error> {
    if tasks.is_empty() {
        println!("No tasks.");
        return Ok(());
    }

    // Build a repo_id -> prefix lookup for display
    let repo_repo = RepoRepo::new(pool);
    let mut prefixes = std::collections::HashMap::new();
    for t in tasks {
        if !prefixes.contains_key(&t.repo_id) {
            if let Some(r) = repo_repo.get(t.repo_id).await? {
                prefixes.insert(t.repo_id, r.task_prefix);
            }
        }
    }

    println!("{:<16} {:<12} {:<3} {:<8} {}", "ID", "STATUS", "P", "KIND", "TITLE");
    for t in tasks {
        let id = format!("{}-{}", prefixes.get(&t.repo_id).cloned().unwrap_or_default(), t.seq);
        let title = if t.title.len() > 60 { format!("{}…", &t.title[..59]) } else { t.title.clone() };
        println!(
            "{:<16} {:<12} P{:<2} {:<8} {}",
            id, t.status.to_string(), t.priority, t.kind.to_string(), title
        );
    }
    println!("\n{} task(s).", tasks.len());
    Ok(())
}
