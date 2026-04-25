use uuid::Uuid;

use crate::config::AppConfig;
use crate::executor::estimate_tokens;
use crate::models::agent::AgentRepo;
use crate::models::node::{Node, NodeKind, NodeRepo};
use crate::ollama::OllamaClient;

/// Monitors context pressure and triggers digest generation when threshold is exceeded.
pub struct PressureMonitor<'a> {
    node_repo: &'a NodeRepo<'a>,
    agent_repo: &'a AgentRepo<'a>,
    ollama: &'a OllamaClient,
    config: &'a AppConfig,
}

impl<'a> PressureMonitor<'a> {
    pub fn new(
        node_repo: &'a NodeRepo<'a>,
        agent_repo: &'a AgentRepo<'a>,
        ollama: &'a OllamaClient,
        config: &'a AppConfig,
    ) -> Self {
        Self {
            node_repo,
            agent_repo,
            ollama,
            config,
        }
    }

    /// Calculate total context weight for an agent's current DAG path.
    pub async fn calculate_weight(&self, agent_id: Uuid) -> Result<usize, crate::YggError> {
        let agent = self
            .agent_repo
            .get(agent_id)
            .await?
            .ok_or_else(|| crate::YggError::Config(format!("agent {agent_id} not found")))?;

        match agent.head_node_id {
            Some(head_id) => {
                let tokens = self.node_repo.calculate_path_tokens(head_id).await?;
                Ok(tokens as usize)
            }
            None => Ok(0),
        }
    }

    /// Check if a digest is needed and generate one if so.
    /// Returns the digest Node if a flush occurred.
    pub async fn maybe_flush(&self, agent_id: Uuid) -> Result<Option<Node>, crate::YggError> {
        let weight = self.calculate_weight(agent_id).await?;

        if weight < self.config.context_limit_tokens {
            return Ok(None);
        }

        tracing::info!(agent_id = %agent_id, weight, "pressure threshold exceeded, generating digest");

        let context = self.build_active_context(agent_id).await?;

        // Extract key info deterministically first, then summarize via LLM in chunks
        let digest_text = self.generate_chunked_digest(&context).await?;

        let agent = self.agent_repo.get(agent_id).await?.unwrap();
        let digest_node = self
            .node_repo
            .insert(
                agent.head_node_id,
                agent_id,
                NodeKind::Digest,
                serde_json::json!({ "digest": digest_text }),
                estimate_tokens(&digest_text) as i32,
            )
            .await?;

        // Atomic state update — no crash window between set_digest and update_head
        self.agent_repo
            .flush_context(
                agent_id,
                digest_node.id,
                digest_node.id,
                estimate_tokens(&digest_text) as i32,
            )
            .await?;

        tracing::info!(agent_id = %agent_id, digest_id = %digest_node.id, "digest generated");

        Ok(Some(digest_node))
    }

    /// Generate a digest by chunking context to fit within the local model's window.
    /// Step 1: Extract key info deterministically (files changed, tool calls, decisions).
    /// Step 2: If remaining context is small enough, summarize in one call.
    /// Step 3: Otherwise, chunk and summarize each chunk, then merge summaries.
    async fn generate_chunked_digest(&self, context: &[Node]) -> Result<String, crate::YggError> {
        // Step 1: Deterministic extraction (no LLM needed)
        let mut files_changed: Vec<String> = Vec::new();
        let mut tool_calls: Vec<String> = Vec::new();
        let mut decisions: Vec<String> = Vec::new();

        for node in context {
            match node.kind {
                NodeKind::ToolCall => {
                    if let Some(cmd) = node.content.get("command").and_then(|v| v.as_str()) {
                        tool_calls.push(cmd.to_string());
                    }
                }
                NodeKind::ToolResult => {
                    // Extract file paths from tool results
                    let content_str = node.content.to_string();
                    for word in content_str.split_whitespace() {
                        if word.contains('/')
                            && (word.ends_with(".rs")
                                || word.ends_with(".ts")
                                || word.ends_with(".py")
                                || word.contains("src/"))
                        {
                            files_changed.push(word.trim_matches('"').to_string());
                        }
                    }
                }
                NodeKind::UserMessage | NodeKind::AssistantMessage => {
                    let text = node
                        .content
                        .get("task")
                        .or(node.content.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if text.contains("decided")
                        || text.contains("chose")
                        || text.contains("will use")
                        || text.contains("switched to")
                    {
                        decisions.push(text.chars().take(200).collect());
                    }
                }
                _ => {}
            }
        }

        files_changed.sort();
        files_changed.dedup();
        tool_calls.dedup();

        let deterministic_section = format!(
            "## Files touched\n{}\n\n## Tool calls ({})\n{}\n\n## Key decisions\n{}",
            files_changed
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n"),
            tool_calls.len(),
            tool_calls
                .iter()
                .take(20)
                .map(|t| format!("- {t}"))
                .collect::<Vec<_>>()
                .join("\n"),
            decisions
                .iter()
                .map(|d| format!("- {d}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        // Step 2: Serialize context for LLM summarization
        let context_text: String = context
            .iter()
            .map(|n| format!("[{:?}] {}", n.kind, n.content))
            .collect::<Vec<_>>()
            .join("\n");

        // Max chunk size: ~6k tokens (conservative for 7B models with 8k window)
        let max_chunk_chars = 24_000; // ~6k tokens at 4 chars/token

        if context_text.len() <= max_chunk_chars {
            // Small enough for one call
            let summary = self
                .ollama
                .generate_digest(&context_text)
                .await
                .unwrap_or_else(|_| "LLM summarization unavailable".to_string());
            return Ok(format!("{deterministic_section}\n\n## Summary\n{summary}"));
        }

        // Step 3: Chunk by newlines into groups that fit the model's window
        let lines: Vec<&str> = context_text.lines().collect();
        let mut chunk_summaries = Vec::new();
        let mut current_chunk = String::new();

        for line in &lines {
            if current_chunk.len() + line.len() > max_chunk_chars {
                if !current_chunk.is_empty() {
                    let summary = self
                        .ollama
                        .generate_digest(&current_chunk)
                        .await
                        .unwrap_or_else(|_| "chunk summary unavailable".to_string());
                    chunk_summaries.push(summary);
                    current_chunk.clear();
                }
            }
            current_chunk.push_str(line);
            current_chunk.push('\n');
        }
        if !current_chunk.is_empty() {
            let summary = self
                .ollama
                .generate_digest(&current_chunk)
                .await
                .unwrap_or_else(|_| "chunk summary unavailable".to_string());
            chunk_summaries.push(summary);
        }

        // Merge chunk summaries
        let merged = if chunk_summaries.len() > 1 {
            let combined = chunk_summaries.join("\n---\n");
            if combined.len() <= max_chunk_chars {
                self.ollama
                    .generate_digest(&format!(
                        "Merge these summaries into one coherent summary:\n{combined}"
                    ))
                    .await
                    .unwrap_or(combined)
            } else {
                combined
            }
        } else {
            chunk_summaries.into_iter().next().unwrap_or_default()
        };

        Ok(format!("{deterministic_section}\n\n## Summary\n{merged}"))
    }

    /// Build the active context: all nodes from the last digest (or root) to head.
    pub async fn build_active_context(&self, agent_id: Uuid) -> Result<Vec<Node>, crate::YggError> {
        let agent = self
            .agent_repo
            .get(agent_id)
            .await?
            .ok_or_else(|| crate::YggError::Config(format!("agent {agent_id} not found")))?;

        let head_id = match agent.head_node_id {
            Some(id) => id,
            None => return Ok(vec![]),
        };

        let full_path = self.node_repo.get_ancestor_path(head_id).await?;

        // If there's a digest, only return nodes after it
        if let Some(digest_id) = agent.digest_id {
            let after_digest: Vec<Node> = full_path
                .into_iter()
                .skip_while(|n| n.id != digest_id)
                .collect();
            return Ok(after_digest);
        }

        Ok(full_path)
    }
}
