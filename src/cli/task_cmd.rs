use crate::models::agent::AgentRepo;
use crate::models::event::{EventKind, EventRepo};
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

/// Public wrapper so plan_cmd can share the resolver without duplicating it.
pub async fn resolve_task_public(
    pool: &sqlx::PgPool,
    reference: &str,
) -> Result<Task, anyhow::Error> {
    resolve_task(pool, reference).await
}

/// Parse a task reference: either a UUID, or a "<prefix>-NNN" string.
async fn resolve_task(
    pool: &sqlx::PgPool,
    reference: &str,
) -> Result<Task, anyhow::Error> {
    // Full UUID is always a fast path.
    if let Ok(uuid) = Uuid::parse_str(reference) {
        let t = TaskRepo::new(pool).get(uuid).await?
            .ok_or_else(|| anyhow::anyhow!("task {uuid} not found"))?;
        return Ok(t);
    }

    // Short-UUID shorthand — prefix match on task_id::text. Accepted forms:
    //   baddbb20          (bare hex, ≥6 chars)
    //   ygg-baddbb20      (namespaced)
    // Ambiguous prefixes error out with the candidate count so the user
    // knows to paste more of the UUID.
    let hex_candidate = reference.strip_prefix("ygg-").unwrap_or(reference);
    if hex_candidate.len() >= 6
        && hex_candidate.chars().all(|c| c.is_ascii_hexdigit())
    {
        let matches: Vec<Uuid> = sqlx::query_scalar(
            "SELECT task_id FROM tasks WHERE task_id::text LIKE $1 LIMIT 5"
        )
        .bind(format!("{hex_candidate}%"))
        .fetch_all(pool).await?;
        match matches.len() {
            0 => {} // fall through to prefix-seq resolver
            1 => {
                let t = TaskRepo::new(pool).get(matches[0]).await?
                    .ok_or_else(|| anyhow::anyhow!("task vanished"))?;
                return Ok(t);
            }
            n => anyhow::bail!(
                "ambiguous short-UUID '{reference}' ({n} matches) — paste more characters"
            ),
        }
    }

    // <prefix>-<seq>
    let (prefix, seq_str) = reference.rsplit_once('-')
        .ok_or_else(|| anyhow::anyhow!(
            "expected UUID, ygg-<shortuuid>, or <prefix>-<seq>, got {reference}"
        ))?;
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
    /// Emit JSON to stdout instead of the human confirmation line.
    pub json: bool,
}

pub async fn create(pool: &sqlx::PgPool, opts: CreateOpts<'_>) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let created_by = resolve_agent_id(pool, opts.agent_name).await?;

    // Auto-classify (yggdrasil-6) — only calls the LLM for the fields the
    // user didn't explicitly pass. Explicit flags always win. Zero cost if
    // Ollama is down (returns None, we fall back to defaults).
    let missing_kind = opts.kind.is_none();
    let missing_priority = opts.priority.is_none();
    let missing_labels = opts.labels.is_empty();
    let suggestion = if missing_kind || missing_priority || missing_labels {
        crate::task_classify::suggest(opts.title, opts.description).await
    } else {
        None
    };

    let kind = match opts.kind {
        Some(k) => TaskKind::from_str(k).map_err(|e| anyhow::anyhow!(e))?,
        None => suggestion.as_ref()
            .and_then(|s| s.kind.as_deref())
            .and_then(|k| TaskKind::from_str(k).ok())
            .unwrap_or_default(),
    };

    let priority = opts.priority
        .or(suggestion.as_ref().and_then(|s| s.priority))
        .unwrap_or(2);
    if !(0..=4).contains(&priority) {
        anyhow::bail!("priority must be between 0 (critical) and 4 (backlog)");
    }

    let suggested_labels: Vec<String> = if opts.labels.is_empty() {
        suggestion.as_ref().map(|s| s.labels.clone()).unwrap_or_default()
    } else {
        Vec::new()
    };
    let labels: &[String] = if opts.labels.is_empty() { &suggested_labels } else { opts.labels };

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
                labels,
            },
        )
        .await?;

    let task_ref = format!("{}-{}", repo.task_prefix, task.seq);
    let _ = EventRepo::new(pool).emit(
        EventKind::TaskCreated,
        opts.agent_name,
        created_by,
        serde_json::json!({
            "ref": task_ref.clone(),
            "title": task.title,
            "kind": task.kind.to_string(),
            "priority": task.priority,
        }),
    ).await;

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "ref": task_ref,
            "task": task,
        }))?);
    } else {
        println!("Created {}  {}", task_ref, task.title);
    }
    Ok(())
}

pub async fn list(
    pool: &sqlx::PgPool,
    all_repos: bool,
    status: Option<&str>,
    labels: &[String],
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let statuses: Vec<TaskStatus> = match status {
        None => vec![],
        Some(s) => s.split(',')
            .map(|piece| piece.trim())
            .filter(|p| !p.is_empty())
            .map(TaskStatus::from_str)
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow::anyhow!(e))?,
    };
    let tasks = TaskRepo::new(pool)
        .list_multi(repo_id, if statuses.is_empty() { None } else { Some(&statuses) })
        .await?;

    // Label filter — AND semantics across multiple labels (task must have
    // all supplied labels). Applied in-memory since the set is already
    // scoped to repo+status.
    let filtered = if labels.is_empty() {
        tasks
    } else {
        let label_set: std::collections::HashSet<&str> =
            labels.iter().map(|s| s.as_str()).collect();
        let task_repo = TaskRepo::new(pool);
        let mut keep = Vec::with_capacity(tasks.len());
        for t in tasks {
            let task_labels = task_repo.labels(t.task_id).await.unwrap_or_default();
            if label_set.iter().all(|l| task_labels.iter().any(|tl| tl == l)) {
                keep.push(t);
            }
        }
        keep
    };
    if json {
        emit_tasks_json(pool, &filtered).await
    } else {
        print_task_table(pool, &filtered).await
    }
}

pub async fn ready(pool: &sqlx::PgPool, json: bool) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let tasks = TaskRepo::new(pool).ready(repo.repo_id).await?;
    if json {
        return emit_tasks_json(pool, &tasks).await;
    }
    if tasks.is_empty() {
        println!("No ready tasks in {}.", repo.name);
        return Ok(());
    }
    print_task_table(pool, &tasks).await
}

pub async fn blocked(pool: &sqlx::PgPool, json: bool) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let tasks = TaskRepo::new(pool).blocked(repo.repo_id).await?;
    if json {
        return emit_tasks_json(pool, &tasks).await;
    }
    if tasks.is_empty() {
        println!("No blocked tasks in {}.", repo.name);
        return Ok(());
    }
    print_task_table(pool, &tasks).await
}

pub async fn show(pool: &sqlx::PgPool, reference: &str, json: bool) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let repo = RepoRepo::new(pool).get(t.repo_id).await?
        .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;
    let labels = TaskRepo::new(pool).labels(t.task_id).await?;
    let deps = TaskRepo::new(pool).deps(t.task_id).await?;

    if json {
        let links = TaskRepo::new(pool).links(t.task_id).await?;
        let deps_json: Vec<_> = deps.iter().map(|d| serde_json::json!({
            "ref": format!("{}-{}", repo.task_prefix, d.seq),
            "task_id": d.task_id,
            "title": d.title,
            "status": d.status.to_string(),
        })).collect();
        let links_json: Vec<_> = links.iter().map(|(k, id)| serde_json::json!({
            "kind": k,
            "target_id": id,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "ref": format!("{}-{}", repo.task_prefix, t.seq),
            "repo": repo.name,
            "task": t,
            "labels": labels,
            "deps": deps_json,
            "links": links_json,
        }))?);
        return Ok(());
    }

    let short_uuid = &t.task_id.to_string()[..8];
    println!("{}-{}  [{}]  P{}  rel={}  {}  (ygg-{short_uuid})",
        repo.task_prefix, t.seq, t.status, t.priority, t.relevance, t.kind);
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
    let links = TaskRepo::new(pool).links(t.task_id).await?;
    if !links.is_empty() {
        println!();
        println!("  Links:");
        for (kind, target_id) in &links {
            // Best-effort target title lookup.
            let row: Option<(i32, Uuid, String)> = sqlx::query_as(
                "SELECT t.seq, t.repo_id, t.title FROM tasks t WHERE t.task_id = $1"
            ).bind(target_id).fetch_optional(pool).await?;
            if let Some((seq, repo_id, title)) = row {
                let prefix = RepoRepo::new(pool).get(repo_id).await?
                    .map(|r| r.task_prefix).unwrap_or_else(|| "?".into());
                println!("    [{kind}]  {prefix}-{seq}  {title}");
            } else {
                println!("    [{kind}]  {target_id}");
            }
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

    let repo = RepoRepo::new(pool).get(t.repo_id).await?;
    let _ = EventRepo::new(pool).emit(
        EventKind::TaskStatusChanged,
        agent_name,
        agent_id,
        serde_json::json!({
            "ref": format!("{}-{}", repo.as_ref().map(|r| r.task_prefix.as_str()).unwrap_or("?"), t.seq),
            "to": status.to_string(),
            "reason": reason,
            "title": t.title,
        }),
    ).await;

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

pub async fn bump(pool: &sqlx::PgPool, reference: &str, delta: i32) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let new_val = TaskRepo::new(pool).bump_relevance(t.task_id, delta).await?;
    let sign = if delta >= 0 { "+" } else { "" };
    println!("{reference} relevance {sign}{delta} → {new_val}");
    Ok(())
}

pub async fn link(
    pool: &sqlx::PgPool,
    from_ref: &str,
    to_ref: &str,
    kind: &str,
) -> Result<(), anyhow::Error> {
    // Accept both the "see-also" and "see_also" spelling.
    let normalized = kind.replace('-', "_");
    let allowed = ["see_also", "superseded_by", "duplicate_of", "related"];
    if !allowed.contains(&normalized.as_str()) {
        anyhow::bail!("unknown link kind '{kind}' — try: {}", allowed.join(", "));
    }
    let a = resolve_task(pool, from_ref).await?;
    let b = resolve_task(pool, to_ref).await?;
    TaskRepo::new(pool).add_link(a.task_id, b.task_id, &normalized).await?;
    println!("{from_ref}  --[{normalized}]-->  {to_ref}");
    Ok(())
}

pub async fn stats(pool: &sqlx::PgPool, all_repos: bool, json: bool) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let s = TaskRepo::new(pool).stats(repo_id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
    } else {
        println!(
            "open: {}    in_progress: {}    blocked: {}    closed: {}",
            s.open, s.in_progress, s.blocked, s.closed
        );
    }
    Ok(())
}

/// JSON emit path shared by list/ready/blocked. Wraps each task with its
/// repo-qualified ref so downstream agents don't have to look up prefixes.
async fn emit_tasks_json(pool: &sqlx::PgPool, tasks: &[Task]) -> Result<(), anyhow::Error> {
    let repo_repo = RepoRepo::new(pool);
    let mut prefixes: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
    for t in tasks {
        if !prefixes.contains_key(&t.repo_id) {
            if let Some(r) = repo_repo.get(t.repo_id).await? {
                prefixes.insert(t.repo_id, r.task_prefix);
            }
        }
    }
    let out: Vec<serde_json::Value> = tasks.iter().map(|t| {
        let prefix = prefixes.get(&t.repo_id).cloned().unwrap_or_default();
        serde_json::json!({
            "ref": format!("{prefix}-{}", t.seq),
            "task": t,
        })
    }).collect();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "count": tasks.len(),
        "results": out,
    }))?);
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
