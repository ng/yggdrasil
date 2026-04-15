use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::executor::estimate_tokens;
use crate::models::agent::{AgentRepo, AgentState};
use crate::models::node::{NodeKind, NodeRepo};
use crate::status::{self, AgentStatus};

/// Observe a Claude Code session by tailing its JSONL transcript.
/// Ingests each message into the ygg DAG as nodes.
pub async fn execute(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let _node_repo = NodeRepo::new(pool);

    // Get or create the agent
    let agent = agent_repo.register(agent_name).await?;
    let agent_id = agent.agent_id;
    agent_repo.transition(agent_id, AgentState::Idle, AgentState::Executing).await?;

    let session_id = crate::status::new_session_id();
    tracing::info!(agent = agent_name, agent_id = %agent_id, "observer started");

    // Find the most recent Claude Code JSONL transcript
    let transcript = find_latest_transcript().await;

    match transcript {
        Some(path) => {
            tracing::info!(path = %path.display(), "tailing transcript");
            tail_transcript(pool, config, &path, agent_id, agent_name, &session_id).await?;
        }
        None => {
            tracing::info!("no transcript found, polling for new sessions...");
            // Poll until a transcript appears
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Some(path) = find_latest_transcript().await {
                    tracing::info!(path = %path.display(), "transcript found, tailing");
                    tail_transcript(pool, config, &path, agent_id, agent_name, &session_id).await?;
                    break;
                }
            }
        }
    }

    // Cleanup
    agent_repo.transition(agent_id, AgentState::Executing, AgentState::Idle).await.ok();
    status::remove_status(&session_id).await;
    tracing::info!(agent = agent_name, "observer stopped");

    Ok(())
}

/// Find the most recent .jsonl file in Claude's project sessions directory.
async fn find_latest_transcript() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let claude_dir = Path::new(&home).join(".claude").join("projects");

    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;

    let mut dirs = tokio::fs::read_dir(&claude_dir).await.ok()?;
    while let Ok(Some(entry)) = dirs.next_entry().await {
        let path = entry.path();
        if !path.is_dir() { continue; }

        let mut files = match tokio::fs::read_dir(&path).await {
            Ok(f) => f,
            Err(_) => continue,
        };
        while let Ok(Some(file)) = files.next_entry().await {
            let fp = file.path();
            if fp.extension().is_some_and(|e| e == "jsonl") {
                if let Ok(meta) = tokio::fs::metadata(&fp).await {
                    if let Ok(modified) = meta.modified() {
                        if newest.as_ref().map_or(true, |(_, t)| modified > *t) {
                            newest = Some((fp, modified));
                        }
                    }
                }
            }
        }
    }

    newest.map(|(p, _)| p)
}

/// Tail a JSONL transcript file, ingesting each line as a DAG node.
async fn tail_transcript(
    pool: &sqlx::PgPool,
    _config: &AppConfig,
    path: &Path,
    agent_id: Uuid,
    agent_name: &str,
    session_id: &str,
) -> Result<(), anyhow::Error> {
    let node_repo = NodeRepo::new(pool);
    let agent_repo = AgentRepo::new(pool);

    let file = tokio::fs::File::open(path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // Track seen message IDs to avoid duplicates
    let mut seen_ids = std::collections::HashSet::new();
    let mut node_count: u32 = 0;

    // Also watch for file growth (tail -f behavior)
    loop {
        match lines.next_line().await? {
            Some(line) => {
                if line.trim().is_empty() { continue; }

                let msg: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // Dedup by message UUID
                let msg_id = msg.get("uuid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !msg_id.is_empty() && !seen_ids.insert(msg_id.clone()) {
                    continue;
                }

                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let kind = match role {
                    "user" => NodeKind::UserMessage,
                    "assistant" => NodeKind::AssistantMessage,
                    _ => NodeKind::System,
                };

                // Extract token count from usage block
                let tokens = msg.get("usage")
                    .and_then(|u| {
                        let input = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let output = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        Some((input + output) as i32)
                    })
                    .unwrap_or_else(|| {
                        estimate_tokens(&line) as i32
                    });

                // Extract tool_use blocks and create separate ToolCall nodes
                let tool_nodes = extract_tool_uses(&msg);

                // Get current head
                let agent = agent_repo.get(agent_id).await?.unwrap();
                let parent_id = agent.head_node_id;

                // Insert the main message node
                let node = node_repo.insert(parent_id, agent_id, kind, msg.clone(), tokens).await?;
                let mut head_id = node.id;
                let mut total_tokens = agent.context_tokens + tokens;
                node_count += 1;

                // Insert tool call nodes as children
                for (tool_name, tool_input) in &tool_nodes {
                    let content = serde_json::json!({
                        "command": tool_name,
                        "input": tool_input,
                    });
                    let tool_node = node_repo.insert(
                        Some(head_id), agent_id, NodeKind::ToolCall, content,
                        estimate_tokens(&tool_input.to_string()) as i32,
                    ).await?;
                    head_id = tool_node.id;
                    total_tokens += estimate_tokens(&tool_input.to_string()) as i32;
                }

                // Update agent head
                agent_repo.update_head(agent_id, head_id, total_tokens).await?;

                // Update status bar
                let _ = status::write_status(session_id, &AgentStatus {
                    state: "executing".to_string(),
                    locks: "none".to_string(),
                    pressure: total_tokens as u32,
                    task: agent_name.to_string(),
                    nodes: node_count,
                    tokens_hr: 0,
                }).await;
            }
            None => {
                // End of file — wait for more data (tail -f)
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Check if agent was interrupted or shut down
                if let Some(agent) = agent_repo.get(agent_id).await? {
                    match agent.current_state {
                        AgentState::HumanOverride | AgentState::Shutdown => {
                            tracing::info!("agent state changed to {}, stopping observer", agent.current_state);
                            break;
                        }
                        _ => {}
                    }
                }

                // Re-open the file to catch new data (handles log rotation)
                let meta = tokio::fs::metadata(path).await;
                if meta.is_err() {
                    tracing::warn!("transcript file disappeared, stopping");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Extract tool_use blocks from a Claude assistant message.
fn extract_tool_uses(msg: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
        return vec![];
    };

    content
        .iter()
        .filter_map(|item| {
            if item.get("type")?.as_str()? == "tool_use" {
                let name = item.get("name")?.as_str()?.to_string();
                let input = item.get("input").cloned().unwrap_or(serde_json::Value::Null);
                Some((name, input))
            } else {
                None
            }
        })
        .collect()
}
