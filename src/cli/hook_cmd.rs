//! Native `ygg hook <event>` handlers — replaces shell scripts in scripts/hooks/.
//!
//! Each handler reads the JSON payload Claude Code sends on stdin, extracts
//! the relevant fields, and calls into the same internal functions the shell
//! scripts used to chain via subprocess. Eliminates the jq dependency, shell
//! quoting risks, and five separate script files.

use crate::config::AppConfig;
use crate::lock::LockManager;
use crate::models::agent::{AgentRepo, AgentWorkflow};
use crate::models::subagent::{AgentCall, StopReport, SubagentRepo};
use clap::Subcommand;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Where SessionStart records which agent a Claude Code session belongs to.
const SESSION_MAP_DIR: &str = "/tmp/ygg";

/// Upper bound on DB work in the subagent hooks. They are bookkeeping only and
/// must never hold up Claude Code.
const SUBAGENT_HOOK_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Subcommand)]
pub enum HookAction {
    /// SessionStart: inject agent context
    SessionStart,
    /// UserPromptSubmit: write prompt node + inject similar context + deliver messages
    PromptSubmit,
    /// PreToolUse: record tool use + heartbeat + conditional lock acquire
    PreToolUse,
    /// PreCompact: digest conversation + re-inject context
    PreCompact,
    /// Stop: digest session + capture outcome + stop-check
    Stop,
    /// SubagentStart: record a native subagent under its parent agent
    SubagentStart,
    /// SubagentStop: mark a native subagent done + record its token totals
    SubagentStop,
}

/// Read stdin, parse common fields, dispatch to the appropriate handler.
pub async fn handle(action: HookAction) -> anyhow::Result<()> {
    // Read the full JSON payload from stdin (Claude Code pipes it in).
    let input = {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    let payload: serde_json::Value =
        serde_json::from_str(&input).unwrap_or_else(|_| serde_json::json!({}));

    // Extract common fields shared across hooks.
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Hooks that fire inside a native subagent carry `agent_id`; those (and
    // the Subagent* events) belong to the parent agent, whose cwd can differ
    // from the subagent's (`isolation: "worktree"`).
    let subagent_ctx = matches!(action, HookAction::SubagentStart | HookAction::SubagentStop)
        || payload_str(&payload, "agent_id").is_some();
    let agent_name = resolve_agent_name(
        std::env::var("YGG_AGENT_NAME").ok(),
        &session_id,
        std::env::current_dir().ok(),
        subagent_ctx,
        read_session_mapping,
    );

    // Record session_id process-globally so downstream DB calls tag correctly.
    // Avoids the unsafe, non-thread-safe std::env::set_var.
    crate::models::event::set_cc_session_id(&session_id);

    match action {
        HookAction::SessionStart => handle_session_start(&agent_name, &session_id, &payload).await,
        HookAction::PromptSubmit => handle_prompt_submit(&agent_name, &payload).await,
        HookAction::PreToolUse => handle_pre_tool_use(&agent_name, &payload).await,
        HookAction::PreCompact => handle_pre_compact(&agent_name, &payload).await,
        HookAction::Stop => handle_stop(&agent_name, &payload).await,
        HookAction::SubagentStart => {
            handle_subagent_start(&agent_name, &session_id, &payload).await
        }
        HookAction::SubagentStop => handle_subagent_stop(&agent_name, &session_id, &payload).await,
    }
}

fn payload_str<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Agent identity for a hook invocation. `YGG_AGENT_NAME` wins (spawned
/// workers export it, and subagent hooks inherit it). Otherwise the cwd
/// basename — except inside a subagent, where the cwd may be the subagent's
/// own worktree: there the SessionStart mapping for the (parent's) session id
/// is used, then the repo that owns a `.claude/worktrees/<name>` cwd.
fn resolve_agent_name(
    env_name: Option<String>,
    session_id: &str,
    cwd: Option<PathBuf>,
    subagent_ctx: bool,
    session_mapping: impl Fn(&str) -> Option<String>,
) -> String {
    if let Some(name) = env_name.filter(|n| !n.is_empty()) {
        return name;
    }
    let mut cwd = cwd;
    if subagent_ctx {
        if !session_id.is_empty() {
            if let Some(name) = session_mapping(session_id) {
                return name;
            }
        }
        cwd = cwd.map(|p| outside_claude_worktree(&p).to_path_buf());
    }
    cwd.and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "ygg".to_string())
}

fn session_mapping_path(session_id: &str) -> PathBuf {
    Path::new(SESSION_MAP_DIR).join(format!("session-{session_id}.agent"))
}

fn read_session_mapping(session_id: &str) -> Option<String> {
    // Session ids are UUIDs; refuse anything that could leave the directory.
    if !session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    std::fs::read_to_string(session_mapping_path(session_id))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `<repo>/.claude/worktrees/<name>/...` → `<repo>`; any other path unchanged.
fn outside_claude_worktree(path: &Path) -> &Path {
    for dir in path.ancestors().skip(1) {
        if dir.file_name().is_some_and(|n| n == "worktrees")
            && let Some(dot_claude) = dir.parent()
            && dot_claude.file_name().is_some_and(|n| n == ".claude")
            && let Some(repo) = dot_claude.parent()
        {
            return repo;
        }
    }
    path
}

/// The parent's agent row, minted on first sight (same rule as `set_tool`).
async fn resolve_parent(pool: &sqlx::PgPool, agent_name: &str) -> anyhow::Result<AgentWorkflow> {
    let repo = AgentRepo::new(pool, crate::db::user_id());
    let persona = std::env::var("YGG_AGENT_PERSONA")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(agent) = repo
        .get_by_name_persona(agent_name, persona.as_deref())
        .await?
    {
        return Ok(agent);
    }
    Ok(repo
        .register_with_persona(agent_name, persona.as_deref())
        .await?)
}

async fn hook_pool(label: &str) -> Option<sqlx::PgPool> {
    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            warn!("hook {label}: config error: {e}");
            return None;
        }
    };
    match crate::db::connect(&config.database).await {
        Ok(p) => Some(p),
        Err(e) => {
            warn!("hook {label}: db pool error: {e}");
            None
        }
    }
}

// ── SessionStart ────────────────────────────────────────────────────────────

/// Port of scripts/hooks/session-start.sh:
/// 1. Write agent→session mapping to /tmp/ygg/session-{sid}.agent
/// 2. Call prime (output to stdout)
async fn handle_session_start(
    agent_name: &str,
    session_id: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Write session mapping so other hooks/tools can resolve agent from sid.
    if !session_id.is_empty() {
        std::fs::create_dir_all(SESSION_MAP_DIR).ok();
        std::fs::write(session_mapping_path(session_id), agent_name).ok();
    }

    // Call prime — output goes to stdout for Claude Code injection.
    let tp = if transcript_path.is_empty() {
        None
    } else {
        Some(transcript_path.as_str())
    };
    crate::cli::prime::execute(agent_name, tp).await?;

    Ok(())
}

// ── UserPromptSubmit ────────────────────────────────────────────────────────

/// UserPromptSubmit hook:
/// 1. Record incremental token stats from the live transcript
/// 2. Check inbox — if non-empty and not "inbox empty", output to stdout
/// 3. Call mark-read (silent)
async fn handle_prompt_submit(
    agent_name: &str,
    _payload: &serde_json::Value,
) -> anyhow::Result<()> {
    // Gracefully ignore errors (matches `|| true` in the shell script).
    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            warn!("hook prompt-submit: config error: {e}");
            return Ok(());
        }
    };
    let pool = match crate::db::connect(&config.database).await {
        Ok(p) => p,
        Err(e) => {
            warn!("hook prompt-submit: db pool error: {e}");
            return Ok(());
        }
    };

    // Record incremental token stats from the live transcript (ygg-2).
    // Uses replace semantics so repeated calls overwrite (no double-counting).
    match crate::stats::collector::find_latest_transcript() {
        Some(tp) => {
            let turns = crate::stats::collector::parse_session(std::path::Path::new(&tp));
            if turns.is_empty() {
                warn!("hook prompt-submit: transcript has 0 parseable turns: {tp}");
            } else {
                let usage = crate::stats::collector::aggregate_usage(&turns);
                let tool_names: Vec<String> =
                    turns.iter().flat_map(|t| t.tool_names.clone()).collect();
                let category = crate::stats::classifier::classify(&tool_names);
                let agent_repo = AgentRepo::new(&pool, crate::db::user_id());
                match agent_repo.get_by_name(agent_name).await {
                    Ok(Some(agent)) => {
                        if let Err(e) = crate::stats::tracker::replace_stats(
                            &pool,
                            agent.agent_id,
                            &usage,
                            &category,
                        )
                        .await
                        {
                            warn!("hook prompt-submit: replace_stats failed: {e}");
                        }
                    }
                    Ok(None) => {
                        warn!("hook prompt-submit: agent '{agent_name}' not found for stats")
                    }
                    Err(e) => warn!("hook prompt-submit: agent lookup failed: {e}"),
                }
            }
        }
        None => warn!("hook prompt-submit: no transcript found for stats"),
    }

    // Inject unread agent-to-agent messages and advance cursor.
    match crate::cli::msg_cmd::inbox(&pool, agent_name, false).await {
        Ok(msgs) => {
            if !msgs.is_empty() {
                crate::cli::msg_cmd::print_inbox(&msgs);
                // Advance cursor so same messages don't resurface.
                if let Err(e) = crate::cli::msg_cmd::mark_read(&pool, agent_name).await {
                    warn!("hook prompt-submit: mark-read failed: {e}");
                }
            }
        }
        Err(e) => {
            warn!("hook prompt-submit: inbox failed: {e}");
        }
    }

    Ok(())
}

// ── PreToolUse ──────────────────────────────────────────────────────────────

/// Port of scripts/hooks/pre-tool-use.sh:
/// 1. Record tool use (agent-tool) — silent
/// 2. Heartbeat — silent
/// 3. For Edit/Write/NotebookEdit with a file path: acquire lock, warn on conflict
/// 4. Always exit 0
fn print_edit_context(lines: &[String]) {
    if !lines.is_empty() {
        println!(
            "{}",
            serde_json::json!({ "hookSpecificOutput": {
                "hookEventName": "PreToolUse", "additionalContext": lines.join("\n")
            }})
        );
    }
}

async fn handle_pre_tool_use(agent_name: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
    let tool = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let file = payload
        .get("tool_input")
        .and_then(|v| v.get("file_path").or_else(|| v.get("path")))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let select_phase = crate::knowledge::timing::Phase::start("hook_selection");
    let mut observed = None;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if matches!(tool, "Edit" | "Write" | "NotebookEdit") && !file.is_empty() {
        match crate::knowledge::runtime::Context::for_edit_hook(
            crate::config::database::process_environment(),
        ) {
            Ok(Some(context)) => {
                let session = payload
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match crate::cli::learning_cmd::local::surface_for_edit(
                    &context,
                    file,
                    &context.default_agent_name,
                    session,
                ) {
                    Ok(emission) => {
                        print_edit_context(&emission.lines);
                        observed = Some((context, emission.applications));
                    }
                    Err(error) => eprintln!("knowledge: edit-time rules unavailable: {error}"),
                }
            }
            Err(error) => {
                eprintln!("knowledge: edit-time rules unavailable: {error}");
            }
            Ok(None) => {
                eprintln!("knowledge: not initialized; run `ygg init` to create the OKF corpus")
            }
        }
    }
    drop(select_phase);
    let coordination_phase = crate::knowledge::timing::Phase::start("hook_coordination");
    // Knowledge is emitted independently. Coordination can still acquire shared
    // locks while healthy, but a failed query cannot indefinitely delay the hook.
    if let Ok(Ok(Some(pool))) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        pre_tool_coordination(agent_name, payload),
    )
    .await
    {
        drop(coordination_phase);
        let _usage_phase = crate::knowledge::timing::Phase::start("hook_usage");
        if let Some((context, applications)) =
            observed.filter(|(_, applications)| !applications.is_empty())
        {
            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                context.storage_lease(&pool),
            )
            .await
            {
                Ok(Ok(lease)) => {
                    crate::knowledge::usage::after_emission(&context, lease, &applications).await
                }
                _ => eprintln!("knowledge: optional usage telemetry unavailable"),
            }
        }
    }
    Ok(())
}

async fn pre_tool_coordination(
    agent_name: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<Option<sqlx::PgPool>> {
    let tool_name = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let file_path = payload
        .get("tool_input")
        .and_then(|ti| {
            ti.get("file_path")
                .or_else(|| ti.get("path"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("");

    // Best-effort DB connection — if unavailable, exit silently.
    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    let pool = match crate::db::connect(&config.database).await {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };

    // Record tool use for dashboard visibility (silent, ignore errors).
    if !tool_name.is_empty() {
        let _ = crate::cli::agent_cmd::set_tool(&pool, agent_name, tool_name).await;
    }

    // ADR 0020: Agent tool calls and tool calls made inside native subagents.
    if let Err(e) = record_subagent_activity(&pool, agent_name, payload).await {
        warn!("hook pre-tool-use: subagent bookkeeping failed: {e}");
    }

    // ADR 0016 / yggdrasil-99: bump heartbeat on running task_run.
    let _ = crate::cli::run_cmd::heartbeat_cli(&pool, None, agent_name).await;

    // Watcher-of-last-resort: bake an opportunistic maintenance tick into the
    // hook that fires on every tool/ygg call, so a fleet self-heals even when
    // nobody runs a dashboard or `ygg watcher` daemon. Heavily throttled — the
    // atomic claim means at most one full tick per interval across all
    // sessions, and when a supervisor is up this is a single UPDATE that
    // matches nothing. Best-effort; opt out with YGG_HOOK_WATCHER_TICK=0,
    // tune the floor with YGG_HOOK_WATCHER_TICK_SECS (default 60).
    let tick_off = std::env::var("YGG_HOOK_WATCHER_TICK")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
        .unwrap_or(false);
    if !tick_off {
        // Clamp to the 60s floor: a configured 0 or negative value would flow
        // into make_interval and defeat the throttle (a negative interval makes
        // the atomic claim match on every call → stampede).
        let secs = std::env::var("YGG_HOOK_WATCHER_TICK_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(60.0)
            .max(60.0);
        // Bound the tick so a slow git/gh/tmux probe inside a claimed tick can
        // never stall the agent's tool path. The claim already bumped
        // last_tick_at, so dropping the tick on timeout just defers the rest of
        // the maintenance to the next window — never blocks the tool call.
        let watcher = crate::watcher::Watcher::new(pool.clone(), config.clone());
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            watcher.run_once_throttled(secs),
        )
        .await;
    }

    // Only lock on file-modifying tools.
    match tool_name {
        "Edit" | "Write" | "NotebookEdit" => {
            if file_path.is_empty() {
                return Ok(Some(pool));
            }

            let agent_repo = AgentRepo::new(&pool, crate::db::user_id());
            let agent = match agent_repo.get_by_name(agent_name).await {
                Ok(Some(a)) => a,
                _ => return Ok(Some(pool)),
            };

            let lock_mgr = LockManager::new(&pool, config.lock_ttl_secs, crate::db::user_id());
            match lock_mgr.acquire(file_path, agent.agent_id).await {
                Ok(_) => {} // Lock acquired silently.
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("locked by") {
                        eprintln!("ygg: {msg}");
                    }
                }
            }
        }
        _ => {}
    }

    Ok(Some(pool))
}

/// The native Agent tool call in a PreToolUse payload (`Task` is its former
/// name), or `None` for any other tool.
fn agent_call(payload: &serde_json::Value) -> Option<AgentCall<'_>> {
    let tool = payload_str(payload, "tool_name")?;
    if tool != "Agent" && tool != "Task" {
        return None;
    }
    let input = payload.get("tool_input")?;
    let field = |k: &str| {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    Some(AgentCall {
        tool_use_id: payload_str(payload, "tool_use_id")?,
        // Claude Code runs `general-purpose` when the call names no type.
        agent_type: field("subagent_type").unwrap_or("general-purpose"),
        description: field("description"),
        background: input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        isolation: field("isolation"),
    })
}

async fn record_subagent_activity(
    pool: &sqlx::PgPool,
    agent_name: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    let Some(session_id) = payload_str(payload, "session_id") else {
        return Ok(());
    };
    let repo = SubagentRepo::new(pool);
    if let (Some(cc_agent_id), Some(tool)) = (
        payload_str(payload, "agent_id"),
        payload_str(payload, "tool_name"),
    ) {
        repo.touch(session_id, cc_agent_id, tool).await?;
    }
    if let Some(call) = agent_call(payload) {
        let parent = resolve_parent(pool, agent_name).await?;
        repo.record_call(parent.agent_id, crate::db::user_id(), session_id, &call)
            .await?;
    }
    Ok(())
}

// ── SubagentStart / SubagentStop ────────────────────────────────────────────

/// SubagentStart (ADR 0020): record the subagent as a running child of the
/// parent agent. Fail-open: every error is logged and swallowed, and nothing
/// is written to stdout, so the hook can never block or steer Claude Code.
async fn handle_subagent_start(
    agent_name: &str,
    session_id: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    let Some(cc_agent_id) = payload_str(payload, "agent_id") else {
        return Ok(());
    };
    if session_id.is_empty() {
        return Ok(());
    }
    let agent_type = payload_str(payload, "agent_type").unwrap_or("");
    let cwd = payload_str(payload, "cwd");
    let work = async {
        let Some(pool) = hook_pool("subagent-start").await else {
            return Ok(());
        };
        let parent = resolve_parent(&pool, agent_name).await?;
        SubagentRepo::new(&pool)
            .start(
                parent.agent_id,
                crate::db::user_id(),
                session_id,
                cc_agent_id,
                agent_type,
                cwd,
            )
            .await?;
        anyhow::Ok(())
    };
    match tokio::time::timeout(SUBAGENT_HOOK_BUDGET, work).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("hook subagent-start: {e}"),
        Err(_) => warn!("hook subagent-start: timed out"),
    }
    Ok(())
}

/// SubagentStop (ADR 0020): mark the subagent done and record token totals
/// from its own transcript (`agent_transcript_path`), the way the Stop hook
/// does for sessions. Fail-open like `handle_subagent_start`; it never emits a
/// decision, so it cannot keep a subagent running.
async fn handle_subagent_stop(
    agent_name: &str,
    session_id: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    let Some(cc_agent_id) = payload_str(payload, "agent_id") else {
        return Ok(());
    };
    if session_id.is_empty() {
        return Ok(());
    }
    let agent_type = payload_str(payload, "agent_type").unwrap_or("");
    let transcript = payload_str(payload, "agent_transcript_path").map(expand_home);
    let last_message =
        payload_str(payload, "last_assistant_message").map(|m| truncate_utf8(m, 2000));

    let mut report = StopReport {
        transcript_path: transcript.as_deref(),
        last_message,
        ..Default::default()
    };
    if let Some(path) = transcript.as_deref().map(Path::new).filter(|p| p.is_file()) {
        let turns = crate::stats::collector::parse_session(path);
        let usage = crate::stats::collector::aggregate_usage(&turns);
        let clamp = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        report.input_tokens = clamp(usage.input_tokens);
        report.output_tokens = clamp(usage.output_tokens);
        report.cache_read = clamp(usage.cache_read);
        report.cache_write = clamp(usage.cache_write);
        report.tool_calls = i32::try_from(turns.iter().map(|t| t.tool_names.len()).sum::<usize>())
            .unwrap_or(i32::MAX);
    }

    let work = async {
        let Some(pool) = hook_pool("subagent-stop").await else {
            return Ok(());
        };
        let parent = resolve_parent(&pool, agent_name).await?;
        SubagentRepo::new(&pool)
            .stop(
                parent.agent_id,
                crate::db::user_id(),
                session_id,
                cc_agent_id,
                agent_type,
                &report,
            )
            .await?;
        anyhow::Ok(())
    };
    match tokio::time::timeout(SUBAGENT_HOOK_BUDGET, work).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("hook subagent-stop: {e}"),
        Err(_) => warn!("hook subagent-stop: timed out"),
    }
    Ok(())
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) if !home.is_empty() => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── PreCompact ──────────────────────────────────────────────────────────────

/// Port of scripts/hooks/pre-compact.sh:
/// 1. If transcript exists, digest it (without --stop)
/// 2. Re-inject agent context via prime (output to stdout)
async fn handle_pre_compact(agent_name: &str, _payload: &serde_json::Value) -> anyhow::Result<()> {
    // Re-inject agent context before Claude Code compacts the window.
    crate::cli::prime::execute(agent_name, None).await?;

    Ok(())
}

// ── Stop ────────────────────────────────────────────────────────────────────

/// Port of scripts/hooks/stop.sh:
/// 1. If transcript exists, digest with --stop semantics (end session + release locks)
/// 2. If YGG_RUN_CAPTURE != "0", capture-outcome
/// 3. Run stop-check (output to stdout)
async fn handle_stop(agent_name: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Attempt config + pool. If unavailable, warn but still try stop_check
    // at the end — a transient DB failure must not silently skip the blocker
    // check that prevents premature session exits.
    let db = match AppConfig::from_env() {
        Ok(config) => match crate::db::connect(&config.database).await {
            Ok(pool) => Some((pool, config)),
            Err(e) => {
                warn!("hook stop: db pool error: {e}");
                None
            }
        },
        Err(e) => {
            warn!("hook stop: config error: {e}");
            None
        }
    };

    if let Some((ref pool, ref config)) = db {
        let user_id = crate::db::user_id().to_string();
        // ADR 0020: Claude Code fires Stop while background subagents are
        // still working. Their tool calls run under this agent's identity and
        // locks, so keep the session, locks and task run open until they end.
        let mut live_subagents = 0;

        // 1. On stop, always end session + release locks; token stats are
        //    best-effort and depend on a readable transcript.
        if let Ok(Some(a)) = AgentRepo::new(pool, &user_id).get_by_name(agent_name).await {
            // Record token stats from the transcript to agent_stats (ygg-2).
            if !transcript_path.is_empty() && std::path::Path::new(&transcript_path).is_file() {
                let turns =
                    crate::stats::collector::parse_session(std::path::Path::new(&transcript_path));
                if !turns.is_empty() {
                    let usage = crate::stats::collector::aggregate_usage(&turns);
                    let tool_names: Vec<String> =
                        turns.iter().flat_map(|t| t.tool_names.clone()).collect();
                    let category = crate::stats::classifier::classify(&tool_names);
                    if let Err(e) =
                        crate::stats::tracker::replace_stats(pool, a.agent_id, &usage, &category)
                            .await
                    {
                        warn!("hook stop: replace_stats failed: {e}");
                    }
                }
            }

            live_subagents = SubagentRepo::new(pool)
                .live_count(a.agent_id)
                .await
                .unwrap_or(0);
            if live_subagents > 0 {
                eprintln!(
                    "ygg: {live_subagents} native subagent(s) still running; \
                     keeping this agent's session, locks and task run open"
                );
            } else {
                if let Some(sid) =
                    crate::models::session::resolve_current_session(pool, a.agent_id, None).await
                {
                    let _ = crate::models::session::SessionRepo::new(pool)
                        .end(sid)
                        .await;
                }
                let lock_mgr = LockManager::new(pool, config.lock_ttl_secs, &user_id);
                let _ = lock_mgr.release_all_for_agent(a.agent_id).await;

                // Mark the agent Shutdown. Subsequent prompts re-enter Executing
                // via inject (force_state is unconditional); the watcher's reap
                // pass will close the tmux window once the process is truly gone.
                let _ = AgentRepo::new(pool, &user_id)
                    .force_state(a.agent_id, crate::models::agent::AgentState::Shutdown, None)
                    .await;
            }
        }

        // 2. Capture outcome (unless YGG_RUN_CAPTURE=0).
        let skip_capture = std::env::var("YGG_RUN_CAPTURE")
            .map(|v| v == "0")
            .unwrap_or(false);
        if !skip_capture && live_subagents == 0 {
            let _ = crate::cli::run_cmd::capture_outcome_cli(pool, agent_name, None).await;
        }

        // Headless self-heal: run one watcher tick as the session ends so a
        // fleet with no persistent `ygg watcher` daemon (and no open
        // dashboard) still gets its finished workers reaped. The tick
        // self-guards on the singleton advisory lock — when a daemon or
        // dashboard-spawned watcher already holds it, this returns instantly
        // after a single lock probe, so the common case adds no latency. Only
        // one Stop hook does real work at a time. Best-effort; opt out with
        // YGG_STOP_WATCHER_TICK=0.
        let tick_off = std::env::var("YGG_STOP_WATCHER_TICK")
            .map(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            .unwrap_or(false);
        if !tick_off {
            let watcher = crate::watcher::Watcher::new(pool.clone(), config.clone());
            if let Err(e) = watcher.run_once().await {
                warn!("hook stop: opportunistic watcher tick failed: {e}");
            }
        }
    }

    // 2b. ADR 0017 M4 — learnings-capture nudge. Remind spawned workers, as a
    // session ends, to record any durable correction they received with
    // `ygg learn propose` (lands in the approval gate, fires on nothing until a
    // human approves). Advisory only (stderr, never blocks). Opt out with
    // YGG_LEARN_NUDGE=0. The load-bearing push is the CLAUDE.md directive the
    // model actually reads; this line is for the operator tending the queue.
    let spawned = std::env::var("YGG_SPAWNED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let nudge_off = std::env::var("YGG_LEARN_NUDGE")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
        .unwrap_or(false);
    if spawned && !nudge_off {
        eprintln!(
            "ygg: if you were corrected this session, capture the durable rule: \
             `ygg learn propose \"<rule>\" --file-glob \"<glob>\"` (queues for approval)"
        );
    }

    // 3. Stop-check (output to stdout) — runs regardless of whether
    // digest/capture-outcome succeeded or DB was unavailable.
    match &db {
        Some((pool, _)) => {
            if let Err(e) = crate::cli::stop_check::execute(pool, agent_name).await {
                warn!("hook stop: stop-check failed: {e}");
            }
        }
        None => {
            // DB unavailable — we cannot run the blocker query, but we must
            // not silently swallow the fact that stop_check was skipped.
            warn!("hook stop: stop-check skipped — no DB connection available");
            eprintln!("ygg: warning: stop-check could not run (no DB connection)");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_mapping(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn agent_name_env_wins() {
        let name = resolve_agent_name(
            Some("worker-7".into()),
            "sid",
            Some(PathBuf::from("/r/.claude/worktrees/agent-1")),
            true,
            |_| Some("mapped".into()),
        );
        assert_eq!(name, "worker-7");
    }

    #[test]
    fn agent_name_main_session_uses_cwd_basename() {
        let name = resolve_agent_name(
            None,
            "sid",
            Some(PathBuf::from("/code/yggdrasil")),
            false,
            |_| Some("mapped".into()),
        );
        assert_eq!(name, "yggdrasil");
    }

    #[test]
    fn agent_name_subagent_prefers_session_mapping() {
        let name = resolve_agent_name(
            None,
            "sid",
            Some(PathBuf::from("/code/yggdrasil/.claude/worktrees/agent-1")),
            true,
            |sid| (sid == "sid").then(|| "yggdrasil:reviewer".to_string()),
        );
        assert_eq!(name, "yggdrasil:reviewer");
    }

    #[test]
    fn agent_name_subagent_worktree_falls_back_to_repo() {
        let name = resolve_agent_name(
            None,
            "sid",
            Some(PathBuf::from(
                "/code/yggdrasil/.claude/worktrees/agent-1/src",
            )),
            true,
            no_mapping,
        );
        assert_eq!(name, "yggdrasil");
    }

    #[test]
    fn outside_claude_worktree_leaves_other_paths() {
        let p = Path::new("/code/yggdrasil/.claude");
        assert_eq!(outside_claude_worktree(p), p);
        let p = Path::new("/code/yggdrasil/.claude/worktrees");
        assert_eq!(outside_claude_worktree(p), p);
        let p = Path::new("/code/worktrees/x");
        assert_eq!(outside_claude_worktree(p), p);
    }

    #[test]
    fn session_mapping_rejects_path_traversal() {
        assert_eq!(read_session_mapping("../../etc/passwd"), None);
    }

    #[test]
    fn agent_call_reads_agent_tool_input() {
        let payload = serde_json::json!({
            "tool_name": "Agent",
            "tool_use_id": "toolu_1",
            "tool_input": {
                "description": "probe the readme",
                "prompt": "...",
                "subagent_type": "Explore",
                "run_in_background": true,
                "isolation": "worktree"
            }
        });
        let call = agent_call(&payload).unwrap();
        assert_eq!(call.tool_use_id, "toolu_1");
        assert_eq!(call.agent_type, "Explore");
        assert_eq!(call.description, Some("probe the readme"));
        assert!(call.background);
        assert_eq!(call.isolation, Some("worktree"));
    }

    #[test]
    fn agent_call_defaults_and_legacy_task_name() {
        let payload = serde_json::json!({
            "tool_name": "Task",
            "tool_use_id": "toolu_2",
            "tool_input": {"description": "d", "prompt": "p"}
        });
        let call = agent_call(&payload).unwrap();
        assert_eq!(call.agent_type, "general-purpose");
        assert!(!call.background);
        assert_eq!(call.isolation, None);
    }

    #[test]
    fn agent_call_ignores_other_tools_and_missing_ids() {
        let edit = serde_json::json!({"tool_name": "Edit", "tool_use_id": "t", "tool_input": {}});
        assert!(agent_call(&edit).is_none());
        let no_id = serde_json::json!({"tool_name": "Agent", "tool_input": {}});
        assert!(agent_call(&no_id).is_none());
    }

    #[test]
    fn truncate_utf8_keeps_char_boundaries() {
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("ääää", 3), "ä");
    }

    #[test]
    fn prompt_truncation_ascii() {
        // ASCII: byte length == char length, should truncate to exactly 2000 bytes.
        let long = "a".repeat(3000);
        let truncated = if long.len() <= 2000 {
            long.clone()
        } else {
            let mut end = 2000;
            while !long.is_char_boundary(end) {
                end -= 1;
            }
            long[..end].to_string()
        };
        assert_eq!(truncated.len(), 2000);
    }

    #[test]
    fn prompt_truncation_multibyte() {
        // Multi-byte UTF-8: must not exceed 2000 bytes and must not split a char.
        // "ä" is 2 bytes in UTF-8 — 1500 of them = 3000 bytes.
        let long = "ä".repeat(1500);
        assert_eq!(long.len(), 3000);
        let truncated = if long.len() <= 2000 {
            long.clone()
        } else {
            let mut end = 2000;
            while !long.is_char_boundary(end) {
                end -= 1;
            }
            long[..end].to_string()
        };
        assert!(truncated.len() <= 2000);
        assert!(truncated.len() >= 1998); // snapped to nearest char boundary
        // Must be valid UTF-8 (String guarantees this, but be explicit).
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn empty_payload_parses() {
        let val: serde_json::Value =
            serde_json::from_str("{}").unwrap_or_else(|_| serde_json::json!({}));
        assert!(val.get("session_id").is_none());
        assert!(val.get("prompt").is_none());
    }

    #[test]
    fn tool_input_extraction() {
        let payload: serde_json::Value = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "/src/main.rs"
            }
        });
        let file = payload
            .get("tool_input")
            .and_then(|ti| {
                ti.get("file_path")
                    .or_else(|| ti.get("path"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");
        assert_eq!(file, "/src/main.rs");
    }

    #[test]
    fn tool_input_path_fallback() {
        let payload: serde_json::Value = serde_json::json!({
            "tool_name": "Write",
            "tool_input": {
                "path": "/src/lib.rs"
            }
        });
        let file = payload
            .get("tool_input")
            .and_then(|ti| {
                ti.get("file_path")
                    .or_else(|| ti.get("path"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");
        assert_eq!(file, "/src/lib.rs");
    }

    /// Drift regression: native pre-tool-use handler must call heartbeat
    /// (yggdrasil-99 / yggdrasil-107). The source code is checked at compile
    /// time so removing the call is a loud test failure.
    #[test]
    fn native_pre_tool_use_calls_heartbeat() {
        let src = include_str!("hook_cmd.rs");
        assert!(
            src.contains("heartbeat_cli"),
            "hook_cmd.rs handle_pre_tool_use must call heartbeat_cli (yggdrasil-99)"
        );
    }

    /// Drift regression: native stop handler must call capture_outcome
    /// (yggdrasil-97 / yggdrasil-107).
    #[test]
    fn native_stop_calls_capture_outcome() {
        let src = include_str!("hook_cmd.rs");
        assert!(
            src.contains("capture_outcome_cli"),
            "hook_cmd.rs handle_stop must call capture_outcome_cli (yggdrasil-97)"
        );
    }

    /// Drift regression: native stop handler must call stop_check.
    #[test]
    fn native_stop_calls_stop_check() {
        let src = include_str!("hook_cmd.rs");
        assert!(
            src.contains("stop_check::execute"),
            "hook_cmd.rs handle_stop must call stop_check::execute"
        );
    }

    /// Drift regression: native stop handler must record token stats
    /// (ygg-2). Removing the call silently breaks cost/throughput tracking.
    #[test]
    fn native_stop_calls_replace_stats() {
        let src = include_str!("hook_cmd.rs");
        assert!(
            src.contains("replace_stats"),
            "hook_cmd.rs must call replace_stats (ygg-2)"
        );
    }
}
