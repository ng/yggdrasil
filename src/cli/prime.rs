use crate::{config::AppConfig, db, lock::LockManager, models::agent::AgentRepo};

/// Output agent context as markdown — injected by SessionStart and PreCompact hooks.
/// Gracefully degrades when the DB is unavailable.
pub async fn execute(agent_name: &str) -> Result<(), anyhow::Error> {
    let outcome = try_with_db(agent_name).await;

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
}

async fn try_with_db(agent_name: &str) -> Result<PrimeContext, anyhow::Error> {
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

    Ok(PrimeContext {
        state: agent.current_state.to_string(),
        context_tokens: agent.context_tokens,
        context_limit: config.context_limit_tokens,
        locks,
        other_agents,
    })
}

fn print_rich(agent_name: &str, ctx: &PrimeContext) {
    let pct = if ctx.context_limit > 0 {
        ctx.context_tokens as u64 * 100 / ctx.context_limit as u64
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
    println!(
        "**state** {state}  ·  **context** {bar}{pct}% ({tokens} tok)  ·  **locks** {lock_str}",
        state = ctx.state,
        tokens = ctx.context_tokens,
    );

    if !ctx.other_agents.is_empty() {
        println!();
        println!("**other agents**");
        for (name, state, tokens) in &ctx.other_agents {
            println!("  - `{name}` — {state} ({tokens} tok)");
        }
    }

    if pct > 75 {
        println!();
        println!("> ⚠ context at {pct}% — digest will trigger at 100%");
    }

    println!();
    println!(
        "**commands** \
        `ygg status` · \
        `ygg spawn --task \"...\"` · \
        `ygg lock list` · \
        `ygg interrupt take-over --agent <name>`"
    );
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
}

fn pressure_bar(pct: u64) -> &'static str {
    match pct {
        0..=25 => "░",
        26..=50 => "▒",
        51..=75 => "▓",
        _ => "█",
    }
}
