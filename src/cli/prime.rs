use crate::{
    config::AppConfig,
    db,
    lock::LockManager,
    models::{
        agent::AgentRepo,
        repo::{detect_git_repo, slugify, RepoRepo},
        task::{Task, TaskRepo},
    },
};

/// Output agent context as markdown — injected by SessionStart and PreCompact hooks.
/// Accepts an optional transcript path to estimate context pressure from file size.
/// Gracefully degrades when the DB is unavailable.
pub async fn execute(agent_name: &str, transcript_path: Option<&str>) -> Result<(), anyhow::Error> {
    let outcome = try_with_db(agent_name, transcript_path).await;

    match outcome {
        Ok(ctx) => print_rich(agent_name, &ctx),
        Err(e) => print_degraded(agent_name, &e),
    }

    Ok(())
}

// ── internals ────────────────────────────────────────────────────────────────

struct PrimeContext {
    state: String,
    context_tokens: i32,
    context_limit: usize,
    locks: Vec<String>,
    other_agents: Vec<(String, String, i32)>, // (name, state, tokens)
    last_digest: Option<DigestInfo>,
    node_count: i64,
    transcript_tokens: Option<i64>,
    repo_label: Option<String>,
    ready_tasks: Vec<Task>,
    open_count: i64,
}

struct DigestInfo {
    summary: String,
    turns: i64,
    corrections: i64,
    reinforcements: i64,
    age_secs: i64,
}

async fn try_with_db(agent_name: &str, transcript_path: Option<&str>) -> Result<PrimeContext, anyhow::Error> {
    let config = AppConfig::from_env()?;
    let pool = db::create_pool(&config.database_url).await?;
    let agent_repo = AgentRepo::new(&pool);
    // Register (or touch) this agent so it exists in the DB.
    let agent = agent_repo.register(agent_name).await?;

    let lock_mgr = LockManager::new(&pool, config.lock_ttl_secs);
    let locks = lock_mgr
        .list_agent_locks(agent.agent_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.resource_key)
        .collect();

    let all = agent_repo.list().await.unwrap_or_default();
    let other_agents = all
        .into_iter()
        .filter(|a| a.agent_name != agent_name)
        .map(|a| (a.agent_name, a.current_state.to_string(), a.context_tokens))
        .collect();

    // Count total nodes for this agent
    let node_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM nodes WHERE agent_id = $1"
    )
    .bind(agent.agent_id)
    .fetch_one(&pool)
    .await
    .unwrap_or(0);

    // Get most recent digest for this agent
    let last_digest = sqlx::query_as::<_, (serde_json::Value, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT content, created_at FROM nodes
           WHERE agent_id = $1 AND kind = 'digest'
           ORDER BY created_at DESC LIMIT 1"#
    )
    .bind(agent.agent_id)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten()
    .map(|(content, created_at)| {
        let age = (chrono::Utc::now() - created_at).num_seconds();
        DigestInfo {
            summary: content.get("summary").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            turns: content.get("turn_count").and_then(|t| t.as_i64()).unwrap_or(0),
            corrections: content.get("corrections").and_then(|c| c.as_array()).map(|a| a.len() as i64).unwrap_or(0),
            reinforcements: content.get("reinforcements").and_then(|r| r.as_array()).map(|a| a.len() as i64).unwrap_or(0),
            age_secs: age,
        }
    });

    // Estimate context from transcript file size.
    // JSONL has heavy JSON overhead (~10 chars per semantic token).
    let transcript_tokens = transcript_path.and_then(|p| {
        std::fs::metadata(p).ok().map(|m| (m.len() / 10) as i64)
    });

    // Best-effort: detect current repo, surface a few ready tasks.
    let (repo_label, ready_tasks, open_count) = resolve_repo_context(&pool).await;

    Ok(PrimeContext {
        state: agent.current_state.to_string(),
        context_tokens: agent.context_tokens,
        context_limit: config.context_limit_tokens,
        locks,
        other_agents,
        last_digest,
        node_count,
        transcript_tokens,
        repo_label,
        ready_tasks,
        open_count,
    })
}

async fn resolve_repo_context(pool: &sqlx::PgPool) -> (Option<String>, Vec<Task>, i64) {
    let Ok(cwd) = std::env::current_dir() else { return (None, vec![], 0); };
    let repo_repo = RepoRepo::new(pool);

    // Look up the repo without registering: prime should be side-effect-free
    // at the ID-allocation layer. Registration happens when the user creates a task.
    let repo_opt = if let Some((url, _top, name)) = detect_git_repo(&cwd) {
        let prefix = slugify(&name);
        if let Some(u) = url.as_deref() {
            match repo_repo.get_by_url(u).await {
                Ok(Some(r)) => Some(r),
                _ => repo_repo.get_by_prefix(&prefix).await.ok().flatten(),
            }
        } else {
            repo_repo.get_by_prefix(&prefix).await.ok().flatten()
        }
    } else {
        None
    };

    let Some(repo) = repo_opt else { return (None, vec![], 0); };
    let task_repo = TaskRepo::new(pool);
    let ready = task_repo.ready(repo.repo_id).await.unwrap_or_default();
    let stats = task_repo.stats(Some(repo.repo_id)).await.ok();
    let open_count = stats.map(|s| s.open + s.in_progress).unwrap_or(0);
    (Some(format!("{} ({})", repo.name, repo.task_prefix)), ready, open_count)
}

fn print_rich(agent_name: &str, ctx: &PrimeContext) {
    // Use transcript-based estimate if available, otherwise DB node tokens
    let effective_tokens = ctx.transcript_tokens.unwrap_or(ctx.context_tokens as i64);
    let pct = if ctx.context_limit > 0 {
        effective_tokens as u64 * 100 / ctx.context_limit as u64
    } else {
        0
    };
    let bar = pressure_bar(pct);
    let lock_str = if ctx.locks.is_empty() {
        "none".to_string()
    } else {
        ctx.locks.iter().map(|l| format!("`{l}`")).collect::<Vec<_>>().join(", ")
    };

    println!("<!-- ygg:prime -->");
    println!();
    println!("## Yggdrasil · `{agent_name}`");
    println!();

    // Context line — show transcript estimate if available
    let tok_display = if let Some(t) = ctx.transcript_tokens {
        format!("~{}k tok transcript", t / 1000)
    } else {
        format!("{} tok", ctx.context_tokens)
    };
    println!(
        "**state** {state}  ·  **context** {bar}{pct}% ({tok_display})  ·  **locks** {lock_str}",
        state = ctx.state,
    );

    // Session recovery indicator
    if let Some(ref digest) = ctx.last_digest {
        let age = format_age(digest.age_secs);
        println!();
        println!(
            "**recovered** — prior session ({age}): {} turns, {} corrections, {} reinforcements",
            digest.turns, digest.corrections, digest.reinforcements
        );
        if !digest.summary.is_empty() {
            let summary = if digest.summary.len() > 120 {
                format!("{}…", &digest.summary[..117])
            } else {
                digest.summary.clone()
            };
            println!("> {summary}");
        }
    }

    // Memory stats
    if ctx.node_count > 0 {
        println!();
        println!("**memory** {} nodes stored across sessions", ctx.node_count);
    }

    if !ctx.other_agents.is_empty() {
        println!();
        println!("**other agents**");
        for (name, state, tokens) in &ctx.other_agents {
            println!("  - `{name}` — {state} ({tokens} tok)");
        }
    }

    // Context-pressure guidance — accurate about what Yggdrasil actually does.
    // We do NOT automatically compact or clear; Claude Code owns the window.
    // We DO write a digest on Stop and PreCompact, so no information is lost.
    if pct >= 90 {
        println!();
        println!("> **context at {pct}%** — Claude Code will auto-compact soon.");
        println!("> Before it does: consider `/clear` (fresh window) or `ygg digest --now`");
        println!("> (captures a summary node Yggdrasil will use to re-prime the next session).");
    } else if pct >= 75 {
        println!();
        println!("> context at {pct}% — Yggdrasil digests on Stop / PreCompact automatically; no action required.");
    }

    if let Some(label) = &ctx.repo_label {
        println!();
        println!("**repo** {label} — {} open / in-progress", ctx.open_count);
        if !ctx.ready_tasks.is_empty() {
            println!();
            println!("**ready tasks** (no unsatisfied blockers)");
            for t in ctx.ready_tasks.iter().take(5) {
                let prefix = label.split_once('(')
                    .and_then(|(_, rest)| rest.strip_suffix(')'))
                    .unwrap_or("");
                println!("  - `{}-{}`  P{} [{}]  {}", prefix, t.seq, t.priority, t.status, t.title);
            }
            if ctx.ready_tasks.len() > 5 {
                println!("  … and {} more (`ygg task ready`)", ctx.ready_tasks.len() - 5);
            }
        }
    }

    println!();
    println!("### When to use `ygg`");
    println!();
    println!("- **Finding work** → `ygg task ready` for unblocked tasks in this repo; `ygg task list` for everything.");
    println!("- **Tracking work** → `ygg task create \"...\"` before starting non-trivial work; `ygg task claim <id>` to take one; `ygg task close <id>` when done.");
    println!("- **Persistent memory** → `ygg remember \"...\"` for durable notes the similarity retriever should surface in future sessions.");
    println!("- **Before editing a shared resource** another agent might touch → `ygg lock acquire <key>`. Release when done.");
    println!("- **For parallel work** that warrants its own context window → `ygg spawn --task \"...\"` instead of the native Task/Agent tool.");
    println!("- **To steer or take over** another agent → `ygg interrupt take-over --agent <name>`.");
    println!("- **Before assuming you're alone** → `ygg status` to see other agents' state and locks.");
    println!("- **`[ygg memory | ...]` injections above your user prompts are real prior context** — read them.");
    println!();
    println!("Do **not** use `bd` / beads in this project — `ygg task` replaces it.");
}

fn print_degraded(agent_name: &str, err: &anyhow::Error) {
    println!("<!-- ygg:prime:degraded -->");
    println!();
    println!("**Yggdrasil** · agent `{agent_name}`");
    println!("**db** unavailable ({err})");
    println!();
    println!(
        "Hooks are active (file locks, context injection). \
        Run `ygg init` if the database is not configured."
    );
    println!();
    println!("Once the DB is reachable, `ygg prime` emits agent-coordination rules — for now, \
        coordinate via `ygg lock acquire/release`, `ygg spawn`, `ygg status`. Do not use `bd` / beads.");
}

fn pressure_bar(pct: u64) -> &'static str {
    match pct {
        0..=25 => "░",
        26..=50 => "▒",
        51..=75 => "▓",
        _ => "█",
    }
}

fn format_age(secs: i64) -> String {
    if secs < 60 { return format!("{secs}s ago"); }
    let mins = secs / 60;
    if mins < 60 { return format!("{mins}m ago"); }
    let hours = mins / 60;
    if hours < 24 { return format!("{hours}h ago"); }
    format!("{}d ago", hours / 24)
}
