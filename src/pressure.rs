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
        let context_text = context
            .iter()
            .map(|n| format!("[{:?}] {}", n.kind, n.content))
            .collect::<Vec<_>>()
            .join("\n");

        let digest_text = self.ollama.generate_digest(&context_text).await?;

        let agent = self.agent_repo.get(agent_id).await?.unwrap();
        let digest_node = self.node_repo.insert(
            agent.head_node_id,
            agent_id,
            NodeKind::Digest,
            serde_json::json!({ "digest": digest_text }),
            estimate_tokens(&digest_text) as i32,
        ).await?;

        self.agent_repo.set_digest(agent_id, digest_node.id).await?;
        self.agent_repo.update_head(agent_id, digest_node.id, estimate_tokens(&digest_text) as i32).await?;

        tracing::info!(agent_id = %agent_id, digest_id = %digest_node.id, "digest generated");

        Ok(Some(digest_node))
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
