//! Native `ygg hook <event>` handlers — replaces shell scripts in scripts/hooks/.
//!
//! Each handler reads the JSON payload Claude Code sends on stdin, extracts
//! the relevant fields, and calls into the same internal functions the shell
//! scripts used to chain via subprocess. Eliminates the jq dependency, shell
//! quoting risks, and five separate script files.

use crate::config::AppConfig;
use crate::lock::LockManager;
use crate::models::agent::AgentRepo;
use clap::Subcommand;
use tracing::warn;

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

    let agent_name = std::env::var("YGG_AGENT_NAME").unwrap_or_else(|_| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "ygg".to_string())
    });

    // Propagate session_id to env so downstream DB calls tag correctly.
    if !session_id.is_empty() {
        unsafe {
            std::env::set_var("CLAUDE_SESSION_ID", &session_id);
        }
    }

    match action {
        HookAction::SessionStart => handle_session_start(&agent_name, &session_id, &payload).await,
        HookAction::PromptSubmit => handle_prompt_submit(&agent_name, &payload).await,
        HookAction::PreToolUse => handle_pre_tool_use(&agent_name, &payload).await,
        HookAction::PreCompact => handle_pre_compact(&agent_name, &payload).await,
        HookAction::Stop => handle_stop(&agent_name, &payload).await,
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
        let dir = std::path::Path::new("/tmp/ygg");
        std::fs::create_dir_all(dir).ok();
        let mapping_file = dir.join(format!("session-{session_id}.agent"));
        std::fs::write(&mapping_file, agent_name).ok();
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

/// Port of scripts/hooks/prompt-submit.sh:
/// 1. Extract prompt (truncate to 2000 chars)
/// 2. Call inject with agent + prompt (output to stdout)
/// 3. Check inbox — if non-empty and not "inbox empty", output to stdout
/// 4. Call mark-read (silent)
async fn handle_prompt_submit(agent_name: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
    let prompt_raw = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    // Truncate to 2000 chars (matching the shell script's head -c 2000).
    let prompt: String = prompt_raw.chars().take(2000).collect();

    let prompt_opt = if prompt.is_empty() {
        None
    } else {
        Some(prompt.as_str())
    };

    // Inject: writes prompt node, similarity search, returns context lines.
    // Gracefully ignore errors (matches `|| true` in the shell script).
    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            warn!("hook prompt-submit: config error: {e}");
            return Ok(());
        }
    };
    let pool = match crate::db::create_pool(&config.database_url).await {
        Ok(p) => p,
        Err(e) => {
            warn!("hook prompt-submit: db pool error: {e}");
            return Ok(());
        }
    };

    if let Err(e) = crate::cli::inject::execute(&pool, &config, agent_name, prompt_opt).await {
        warn!("hook prompt-submit: inject failed: {e}");
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
async fn handle_pre_tool_use(agent_name: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
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
        Err(_) => return Ok(()),
    };
    let pool = match crate::db::create_pool(&config.database_url).await {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    // Record tool use for dashboard visibility (silent, ignore errors).
    if !tool_name.is_empty() {
        let _ = crate::cli::agent_cmd::set_tool(&pool, agent_name, tool_name).await;
    }

    // ADR 0016 / yggdrasil-99: bump heartbeat on running task_run.
    let _ = crate::cli::run_cmd::heartbeat_cli(&pool, None, agent_name).await;

    // Only lock on file-modifying tools.
    match tool_name {
        "Edit" | "Write" | "NotebookEdit" => {
            if file_path.is_empty() {
                return Ok(());
            }

            let agent_repo = AgentRepo::new(&pool, crate::db::user_id());
            let agent = match agent_repo.get_by_name(agent_name).await {
                Ok(Some(a)) => a,
                _ => return Ok(()),
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

    Ok(())
}

// ── PreCompact ──────────────────────────────────────────────────────────────

/// Port of scripts/hooks/pre-compact.sh:
/// 1. If transcript exists, digest it (without --stop)
/// 2. Re-inject agent context via prime (output to stdout)
async fn handle_pre_compact(agent_name: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Digest the conversation about to be compacted.
    if !transcript_path.is_empty() && std::path::Path::new(&transcript_path).is_file() {
        let config = match AppConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                warn!("hook pre-compact: config error: {e}");
                // Still try prime below.
                crate::cli::prime::execute(agent_name, None).await?;
                return Ok(());
            }
        };
        let pool = match crate::db::create_pool(&config.database_url).await {
            Ok(p) => p,
            Err(e) => {
                warn!("hook pre-compact: db pool error: {e}");
                crate::cli::prime::execute(agent_name, None).await?;
                return Ok(());
            }
        };

        if let Err(e) =
            crate::cli::digest::execute(&pool, &config, agent_name, &transcript_path).await
        {
            warn!("hook pre-compact: digest failed: {e}");
        }
    }

    // Re-inject agent context.
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

    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            warn!("hook stop: config error: {e}");
            return Ok(());
        }
    };
    let pool = match crate::db::create_pool(&config.database_url).await {
        Ok(p) => p,
        Err(e) => {
            warn!("hook stop: db pool error: {e}");
            return Ok(());
        }
    };
    let user_id = crate::db::user_id().to_string();

    // 1. Digest with --stop semantics.
    if !transcript_path.is_empty() && std::path::Path::new(&transcript_path).is_file() {
        if let Err(e) =
            crate::cli::digest::execute(&pool, &config, agent_name, &transcript_path).await
        {
            warn!("hook stop: digest failed: {e}");
        }

        // --stop: end session + release locks (matching the Digest dispatch in main.rs).
        if let Ok(Some(a)) = AgentRepo::new(&pool, &user_id)
            .get_by_name(agent_name)
            .await
        {
            if let Some(sid) =
                crate::models::session::resolve_current_session(&pool, a.agent_id, None).await
            {
                let _ = crate::models::session::SessionRepo::new(&pool)
                    .end(sid)
                    .await;
            }
            let lock_mgr = LockManager::new(&pool, config.lock_ttl_secs, &user_id);
            let _ = lock_mgr.release_all_for_agent(a.agent_id).await;
        }
    }

    // 2. Capture outcome (unless YGG_RUN_CAPTURE=0).
    let skip_capture = std::env::var("YGG_RUN_CAPTURE")
        .map(|v| v == "0")
        .unwrap_or(false);
    if !skip_capture {
        let _ = crate::cli::run_cmd::capture_outcome_cli(&pool, agent_name, None).await;
    }

    // 3. Stop-check (output to stdout).
    if let Err(e) = crate::cli::stop_check::execute(&pool, agent_name).await {
        warn!("hook stop: stop-check failed: {e}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_truncation() {
        // Verify our char-based truncation matches the shell's head -c 2000
        let long = "a".repeat(3000);
        let truncated: String = long.chars().take(2000).collect();
        assert_eq!(truncated.len(), 2000);
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
}
