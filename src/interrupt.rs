use uuid::Uuid;

use crate::config::AppConfig;
use crate::executor::estimate_tokens;
use crate::models::agent::{AgentRepo, AgentState};
use crate::models::node::{NodeKind, NodeRepo};
use crate::ollama::OllamaClient;
use crate::pressure::PressureMonitor;
use crate::tmux::TmuxManager;

/// Snapshot of agent state at the moment of interruption.
pub struct InterruptSnapshot {
    pub agent_id: Uuid,
    pub prior_state: AgentState,
    pub head_node_id: Option<Uuid>,
    pub digest_id: Option<Uuid>,
    pub context_tokens: i32,
}

/// Take over an agent's session.
/// 1. Force a digest (so nothing is lost)
/// 2. Transition to HumanOverride
/// 3. Focus the tmux window
pub async fn take_over(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    agent_name: &str,
) -> Result<InterruptSnapshot, anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let node_repo = NodeRepo::new(pool);
    let ollama = OllamaClient::new(
        &config.ollama_base_url,
        &config.ollama_embed_model,
        &config.ollama_chat_model,
    );

    let agent = agent_repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

    let snapshot = InterruptSnapshot {
        agent_id: agent.agent_id,
        prior_state: agent.current_state.clone(),
        head_node_id: agent.head_node_id,
        digest_id: agent.digest_id,
        context_tokens: agent.context_tokens,
    };

    // Force a digest before handover
    let pressure = PressureMonitor::new(&node_repo, &agent_repo, &ollama, config);
    let _ = pressure.maybe_flush(agent.agent_id).await;

    // Transition to HumanOverride
    agent_repo
        .transition(
            agent.agent_id,
            agent.current_state.clone(),
            AgentState::HumanOverride,
        )
        .await?;

    // Insert a human_override node
    if let Some(head_id) = agent.head_node_id {
        let node = node_repo
            .insert(
                Some(head_id),
                agent.agent_id,
                NodeKind::HumanOverride,
                serde_json::json!({ "action": "take_over", "prior_state": agent.current_state.to_string() }),
                0,
            )
            .await?;
        agent_repo
            .update_head(agent.agent_id, node.id, agent.context_tokens)
            .await?;
    }

    // Focus the tmux window
    TmuxManager::select_window(agent_name).await.ok();

    tracing::info!(agent = agent_name, "human override initiated");

    Ok(snapshot)
}

/// Hand back control to the agent.
/// 1. Insert a summary node of what the human did
/// 2. Transition back to Idle
pub async fn hand_back(
    pool: &sqlx::PgPool,
    agent_name: &str,
    summary: &str,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let node_repo = NodeRepo::new(pool);

    let agent = agent_repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

    if agent.current_state != AgentState::HumanOverride {
        anyhow::bail!(
            "agent '{}' is in state '{}', not human_override",
            agent_name,
            agent.current_state
        );
    }

    // Insert summary node
    if let Some(head_id) = agent.head_node_id {
        let content = serde_json::json!({
            "action": "hand_back",
            "summary": summary,
        });
        let node = node_repo
            .insert(
                Some(head_id),
                agent.agent_id,
                NodeKind::HumanOverride,
                content,
                estimate_tokens(summary) as i32,
            )
            .await?;
        agent_repo
            .update_head(
                agent.agent_id,
                node.id,
                agent.context_tokens + estimate_tokens(summary) as i32,
            )
            .await?;
    }

    // Transition back to idle
    agent_repo
        .transition(agent.agent_id, AgentState::HumanOverride, AgentState::Idle)
        .await?;

    tracing::info!(agent = agent_name, "control handed back");

    Ok(())
}
