use crate::{
    config::AppConfig,
    db,
    lock::LockManager,
    models::{
        agent::AgentRepo,
        handoff::{Handoff, HandoffRepo},
        memory::{Memory, MemoryRepo},
        repo::{RepoRepo, detect_git_repo, slugify},
        task::{Task, TaskRepo},
    },
};
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Output agent context as markdown — injected by SessionStart and PreCompact hooks.
/// Accepts an optional transcript path to estimate context pressure from file size.
/// Gracefully degrades when the DB is unavailable.
pub async fn execute(agent_name: &str, transcript_path: Option<&str>) -> Result<(), anyhow::Error> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let knowledge =
        crate::knowledge::runtime::Context::from_environment(std::env::vars().collect());
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let legacy_notes = matches!(&knowledge, Ok(None));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let legacy_notes = true;

    // Bound the whole coordination read, not just TCP connection setup. A hung
    // database query must not indefinitely suppress local knowledge or hooks.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        try_with_db(agent_name, transcript_path, legacy_notes),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("coordination read timed out")));
    let mut local_notes = Vec::new();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    match &knowledge {
        Ok(Some(context)) => match local_prime_notes(context) {
            Ok(notes) => local_notes = notes,
            Err(error) => eprintln!("ygg prime: local knowledge unavailable: {error}"),
        },
        Err(error) => eprintln!("ygg prime: local knowledge unavailable: {error}"),
        Ok(None) => {}
    }
    match outcome {
        Ok(mut ctx) => {
            if !legacy_notes {
                ctx.notes = local_notes;
            }
            print_rich(agent_name, &ctx);
        }
        Err(error) => {
            print_degraded(agent_name, &error);
            print_notes(&local_notes);
        }
    }

    Ok(())
}

// ── internals ────────────────────────────────────────────────────────────────

struct PrimeNote {
    text: String,
    global: bool,
}
impl From<Memory> for PrimeNote {
    fn from(note: Memory) -> Self {
        Self {
            text: note.text,
            global: note.repo_id.is_none(),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn local_prime_notes(
    context: &crate::knowledge::runtime::Context,
) -> anyhow::Result<Vec<PrimeNote>> {
    // An unknown repo cannot inherit another repo's notes. Explicit globals are
    // still available; the diagnostic distinguishes this from a mapped checkout.
    let repo = match context.repo(&std::env::current_dir()?) {
        Ok(repo) => Some(repo),
        Err(error) => {
            eprintln!("ygg prime: repository knowledge omitted: {error}");
            None
        }
    };
    let selected = context.service.prime_notes(repo, Utc::now())?;
    for diagnostic in selected.diagnostics {
        eprintln!("knowledge: {diagnostic}");
    }
    let current = context
        .service
        .revalidate_notes(&selected.documents, repo, Utc::now())?;
    for diagnostic in current.diagnostics {
        eprintln!("knowledge: {diagnostic}");
    }
    Ok(current
        .documents
        .into_iter()
        .map(|doc| PrimeNote {
            text: doc.document.body,
            global: doc.key.repo.is_none(),
        })
        .collect())
}

struct PrimeContext {
    state: String,
    context_tokens: i32,
    context_limit: usize,
    locks: Vec<String>,
    other_agents: Vec<(String, String, i32)>, // (name, state, tokens)
    transcript_tokens: Option<i64>,
    repo_label: Option<String>,
    ready_tasks: Vec<Task>,
    open_count: i64,
    notes: Vec<PrimeNote>,
    handoff_unavailable: bool,
    handoff: Option<Handoff>,
    pending_migrations: usize,
}

async fn try_with_db(
    agent_name: &str,
    transcript_path: Option<&str>,
    legacy_notes: bool,
) -> Result<PrimeContext, anyhow::Error> {
    let config = AppConfig::from_env()?;
    let pool = db::connect(&config.database).await?;
    let agent_repo = AgentRepo::new(&pool, crate::db::user_id());
    // Register (or touch) this agent so it exists in the DB. Persona from
    // $YGG_AGENT_PERSONA forms a compound key with agent_name — same cwd,
    // different role = different agent row.
    let persona = std::env::var("YGG_AGENT_PERSONA")
        .ok()
        .filter(|s| !s.is_empty());
    let agent = agent_repo
        .register_with_persona(agent_name, persona.as_deref())
        .await?;

    let lock_mgr = LockManager::new(&pool, config.lock_ttl_secs, crate::db::user_id());
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

    // Estimate context from transcript file size.
    // JSONL has heavy JSON overhead (~10 chars per semantic token).
    let transcript_tokens =
        transcript_path.and_then(|p| std::fs::metadata(p).ok().map(|m| (m.len() / 10) as i64));

    // Best-effort: detect current repo, surface a few ready tasks + recent notes.
    let (repo_label, repo_id, ready_tasks, open_count, notes) =
        resolve_repo_context(&pool, legacy_notes).await;

    // Resume note from a prior session of this agent in this repo (`ygg handoff`).
    let handoff_result = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        HandoffRepo::new(&pool).latest(repo_id, Some(agent.agent_id)),
    )
    .await;
    let (handoff, handoff_unavailable) = match handoff_result {
        Ok(Ok(handoff)) => (handoff, false),
        _ => (None, true),
    };

    let pending_migrations = db::pending_migrations(&pool)
        .await
        .map(|v| v.len())
        .unwrap_or(0);

    Ok(PrimeContext {
        state: agent.current_state.to_string(),
        context_tokens: agent.context_tokens,
        context_limit: config.context_limit_tokens,
        locks,
        other_agents,
        transcript_tokens,
        repo_label,
        ready_tasks,
        open_count,
        notes: notes.into_iter().map(PrimeNote::from).collect(),
        handoff_unavailable,
        handoff,
        pending_migrations,
    })
}

async fn resolve_repo_context(
    pool: &sqlx::PgPool,
    legacy_notes: bool,
) -> (Option<String>, Option<Uuid>, Vec<Task>, i64, Vec<Memory>) {
    // Recent global notes show even outside a known repo.
    let global_notes = || async {
        if !legacy_notes {
            return Vec::new();
        }
        MemoryRepo::new(pool)
            .list(None, false, 5)
            .await
            .unwrap_or_default()
    };
    let Ok(cwd) = std::env::current_dir() else {
        return (None, None, vec![], 0, global_notes().await);
    };
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

    let Some(repo) = repo_opt else {
        return (None, None, vec![], 0, global_notes().await);
    };
    let task_repo = TaskRepo::new(pool);
    let ready = task_repo.ready(repo.repo_id).await.unwrap_or_default();
    let stats = task_repo.stats(Some(repo.repo_id)).await.ok();
    let open_count = stats.map(|s| s.open + s.in_progress).unwrap_or(0);
    let notes = if legacy_notes {
        MemoryRepo::new(pool)
            .list(Some(repo.repo_id), false, 5)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    (
        Some(format!("{} ({})", repo.name, repo.task_prefix)),
        Some(repo.repo_id),
        ready,
        open_count,
        notes,
    )
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
        ctx.locks
            .iter()
            .map(|l| format!("`{l}`"))
            .collect::<Vec<_>>()
            .join(", ")
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

    // Resume note from a prior session — the most important thing on a fresh
    // window, so it leads. Supersede with `ygg handoff save`, drop with `clear`.
    if let Some(h) = &ctx.handoff {
        println!();
        println!(
            "## ⏎ Resume here — handoff from {}",
            humanize_age(h.created_at)
        );
        println!();
        println!("{}", h.text);
        println!();
        println!("*(supersede with `ygg handoff save`, dismiss with `ygg handoff clear`)*");
    }

    if ctx.handoff_unavailable {
        println!("\n**handoff** unavailable; local knowledge is independent.");
    }

    if ctx.pending_migrations > 0 {
        println!();
        println!(
            "**⚠ migrations** {} pending — run `ygg migrate` or `ygg init`",
            ctx.pending_migrations,
        );
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
        println!(
            "> context at {pct}% — Yggdrasil digests on Stop / PreCompact automatically; no action required."
        );
    }

    if let Some(label) = &ctx.repo_label {
        println!();
        println!("**repo** {label} — {} open / in-progress", ctx.open_count);
        if !ctx.ready_tasks.is_empty() {
            println!();
            println!("**ready tasks** (no unsatisfied blockers)");
            for t in ctx.ready_tasks.iter().take(5) {
                let prefix = label
                    .split_once('(')
                    .and_then(|(_, rest)| rest.strip_suffix(')'))
                    .unwrap_or("");
                println!(
                    "  - `{}-{}`  P{} [{}]  {}",
                    prefix, t.seq, t.priority, t.status, t.title
                );
            }
            if ctx.ready_tasks.len() > 5 {
                println!(
                    "  … and {} more (`ygg task ready`)",
                    ctx.ready_tasks.len() - 5
                );
            }
        }
    }

    print_notes(&ctx.notes);

    println!();
    println!("### When to use `ygg`");
    println!();
    println!(
        "- **Finding work** → `ygg task ready` for unblocked tasks in this repo; `ygg task list` for everything."
    );
    println!(
        "- **Tracking work** → `ygg task create \"...\" --kind <task|bug|feature|chore|epic> --priority <0-4>` (0=critical, 4=backlog; NOT \"high\"/\"medium\"/\"low\"). `ygg task claim <ref>` to take one; `ygg task close <ref>` when done."
    );
    println!(
        "- **Before editing a shared resource** another agent might touch → `ygg lock acquire <key>`. Release when done."
    );
    println!(
        "- **For parallel work** that warrants its own context window → `ygg spawn --task \"...\"` instead of the native Task/Agent tool."
    );
    println!(
        "- **To steer or take over** another agent → `ygg interrupt take-over --agent <name>`."
    );
    println!(
        "- **Before assuming you're alone** → `ygg status` to see other agents' state and locks."
    );
    println!();
    println!("Do **not** use `bd` / beads in this project — `ygg task` replaces it.");
}

fn print_notes(notes: &[PrimeNote]) {
    if notes.is_empty() {
        return;
    }
    println!("\n**notes** (`ygg remember`)");
    for note in notes {
        println!(
            "  - {}{}",
            note_snippet(&note.text),
            if note.global { " · global" } else { "" }
        );
    }
}

fn print_degraded(agent_name: &str, _err: &anyhow::Error) {
    println!("<!-- ygg:prime:degraded -->\n");
    println!("**Yggdrasil** · agent `{agent_name}`");
    println!("**db** unavailable — coordination state and handoff could not be loaded.");
    println!(
        "Restore the configured database connection to use shared locks, tasks and agent status."
    );
}

/// Coarse "Nx ago" age for the handoff header. Best-effort, human-facing.
fn humanize_age(created: DateTime<Utc>) -> String {
    let secs = (Utc::now() - created).num_seconds().max(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// One-line note preview for the prime block — collapse newlines, cap length.
fn note_snippet(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= 120 {
        one_line
    } else {
        one_line.chars().take(120).collect::<String>() + "…"
    }
}

fn pressure_bar(pct: u64) -> &'static str {
    match pct {
        0..=25 => "░",
        26..=50 => "▒",
        51..=75 => "▓",
        _ => "█",
    }
}
