use uuid::Uuid;

use crate::config::AppConfig;
use crate::models::agent::AgentRepo;
use crate::models::node::{Node, NodeRepo};
use crate::ollama::OllamaClient;
use crate::pressure::PressureMonitor;

/// An assembled prompt ready for the agent.
pub struct HighDensityPrompt {
    pub system: String,
    pub context_nodes: Vec<Node>,
    pub directives: Vec<Node>,
    pub estimated_tokens: usize,
}

/// Builds prompts using progressive disclosure: directives near the cursor, not front-loaded.
pub struct PromptBuilder<'a> {
    node_repo: &'a NodeRepo<'a>,
    agent_repo: &'a AgentRepo<'a>,
    ollama: &'a OllamaClient,
    pressure: &'a PressureMonitor<'a>,
    _config: &'a AppConfig,
}

impl<'a> PromptBuilder<'a> {
    pub fn new(
        node_repo: &'a NodeRepo<'a>,
        agent_repo: &'a AgentRepo<'a>,
        ollama: &'a OllamaClient,
        pressure: &'a PressureMonitor<'a>,
        config: &'a AppConfig,
    ) -> Self {
        Self {
            node_repo,
            agent_repo,
            ollama,
            pressure,
            _config: config,
        }
    }

    /// Build the full prompt for an agent turn.
    ///
    /// Progressive disclosure ordering:
    /// 1. Digest (compressed prior context) — background
    /// 2. Active context nodes (recent conversation)
    /// 3. Directive nodes from similarity search — near the cursor
    /// 4. Current input — the cursor
    pub async fn build(
        &self,
        agent_id: Uuid,
        current_input: &str,
    ) -> Result<HighDensityPrompt, crate::YggError> {
        let agent = self
            .agent_repo
            .get(agent_id)
            .await?
            .ok_or_else(|| crate::YggError::Config(format!("agent {agent_id} not found")))?;

        // Get digest content if available
        let mut system = String::new();
        if let Some(digest_id) = agent.digest_id {
            let path = self.node_repo.get_ancestor_path(digest_id).await?;
            if let Some(digest_node) = path.last() {
                system.push_str("## Prior Context (Digest)\n");
                system.push_str(&digest_node.content.to_string());
                system.push_str("\n\n");
            }
        }

        // Get active context
        let context_nodes = self.pressure.build_active_context(agent_id).await?;

        // Retrieve relevant directives via similarity search
        let directives = self.retrieve_directives(current_input, agent_id, 5).await?;

        let estimated_tokens = crate::executor::estimate_tokens(&system)
            + context_nodes.iter().map(|n| n.token_count as usize).sum::<usize>()
            + directives.iter().map(|n| n.token_count as usize).sum::<usize>()
            + crate::executor::estimate_tokens(current_input);

        Ok(HighDensityPrompt {
            system,
            context_nodes,
            directives,
            estimated_tokens,
        })
    }

    /// Find top-k directive nodes via similarity search.
    async fn retrieve_directives(
        &self,
        query_text: &str,
        agent_id: Uuid,
        limit: i32,
    ) -> Result<Vec<Node>, crate::YggError> {
        let query_vec = self.ollama.embed(query_text).await?;
        let results = self
            .node_repo
            .similarity_search(&query_vec, agent_id, limit)
            .await?;
        Ok(results)
    }
}
