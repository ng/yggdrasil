use uuid::Uuid;

use crate::config::AppConfig;
use crate::executor::{Executor, estimate_tokens};
use crate::models::agent::{AgentRepo, AgentState};
use crate::models::node::{NodeKind, NodeRepo};
use crate::ollama::OllamaClient;
use crate::pressure::PressureMonitor;
use crate::prompt::PromptBuilder;
use crate::status::{self, AgentStatus};

/// Main agent run loop.
pub async fn execute(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    name: &str,
    task: Option<&str>,
    session_id: &str,
) -> Result<(), anyhow::Error> {
    let node_repo = NodeRepo::new(pool);
    let agent_repo = AgentRepo::new(pool);
    let ollama = OllamaClient::new(
        &config.ollama_base_url,
        &config.ollama_embed_model,
        &config.ollama_chat_model,
    );
    let _executor = Executor::new(config.rtk_binary_path.clone());
    let pressure = PressureMonitor::new(&node_repo, &agent_repo, &ollama, config);
    let prompt_builder = PromptBuilder::new(&node_repo, &agent_repo, &ollama, &pressure, config);

    // Register or resume agent
    let agent = agent_repo.register(name).await?;
    let agent_id = agent.agent_id;
    tracing::info!(agent_id = %agent_id, name, "agent registered");

    // Transition to executing
    agent_repo
        .transition(agent_id, AgentState::Idle, AgentState::Executing)
        .await?;

    // If task provided, insert as initial node
    if let Some(task_text) = task {
        let content = serde_json::json!({ "task": task_text });
        let node = node_repo
            .insert(
                agent.head_node_id,
                agent_id,
                NodeKind::UserMessage,
                content,
                estimate_tokens(task_text) as i32,
            )
            .await?;
        agent_repo
            .update_head(agent_id, node.id, estimate_tokens(task_text) as i32)
            .await?;

        // Fire-and-forget embedding
        let ollama_clone = OllamaClient::new(
            &config.ollama_base_url,
            &config.ollama_embed_model,
            &config.ollama_chat_model,
        );
        let pool_clone = pool.clone();
        let task_text_owned = task_text.to_string();
        let node_id = node.id;
        tokio::spawn(async move {
            if let Ok(vec) = ollama_clone.embed(&task_text_owned).await {
                let repo = NodeRepo::new(&pool_clone);
                let _ = repo.set_embedding(node_id, vec).await;
            }
        });
    }

    // Update status bar
    write_agent_status(session_id, "executing", "", &agent_repo, agent_id).await;

    // Main loop
    loop {
        let agent = match agent_repo.get(agent_id).await? {
            Some(a) => a,
            None => break,
        };

        // Check for human override or shutdown
        match agent.current_state {
            AgentState::HumanOverride => {
                tracing::info!("agent paused — human override active");
                write_agent_status(session_id, "human_override", "", &agent_repo, agent_id).await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            AgentState::Shutdown => {
                tracing::info!("agent shutting down");
                break;
            }
            AgentState::Idle => {
                tracing::info!("agent idle — no more work");
                break;
            }
            _ => {}
        }

        // Check pressure, maybe flush
        if let Some(digest) = pressure.maybe_flush(agent_id).await? {
            tracing::info!(digest_id = %digest.id, "context flushed");
            write_agent_status(session_id, "context_flush", "", &agent_repo, agent_id).await;
        }

        // Build prompt
        let head = agent_repo.get(agent_id).await?.unwrap();
        let task_desc = task.unwrap_or("continue");
        let prompt = prompt_builder.build(agent_id, task_desc).await?;

        tracing::info!(
            estimated_tokens = prompt.estimated_tokens,
            context_nodes = prompt.context_nodes.len(),
            directives = prompt.directives.len(),
            "prompt built"
        );

        // For now, log what we'd send. The actual Claude API call goes through
        // the agent's Claude Code session (spawned in tmux), not through us.
        // The orchestrator records state; the agent does the reasoning.
        let response_content = serde_json::json!({
            "status": "awaiting_agent_response",
            "prompt_tokens": prompt.estimated_tokens,
            "context_nodes": prompt.context_nodes.len(),
        });

        if let Some(head_id) = head.head_node_id {
            let node = node_repo
                .insert(
                    Some(head_id),
                    agent_id,
                    NodeKind::System,
                    response_content,
                    0,
                )
                .await?;
            agent_repo
                .update_head(agent_id, node.id, prompt.estimated_tokens as i32)
                .await?;
        }

        write_agent_status(session_id, "executing", task_desc, &agent_repo, agent_id).await;

        // In a real loop, we'd wait for the agent's Claude Code session to produce output.
        // For now, transition to idle after one cycle.
        agent_repo
            .transition(agent_id, AgentState::Executing, AgentState::Idle)
            .await?;
        break;
    }

    // Cleanup
    agent_repo
        .transition(agent_id, AgentState::Executing, AgentState::Idle)
        .await
        .ok();
    write_agent_status(session_id, "idle", "", &agent_repo, agent_id).await;

    tracing::info!(agent_id = %agent_id, "agent loop finished");
    Ok(())
}

async fn write_agent_status(
    session_id: &str,
    state: &str,
    task: &str,
    agent_repo: &AgentRepo<'_>,
    agent_id: Uuid,
) {
    let agent = agent_repo.get(agent_id).await.ok().flatten();
    let pressure = agent.as_ref().map(|a| a.context_tokens as u32).unwrap_or(0);

    let _ = status::write_status(
        session_id,
        &AgentStatus {
            state: state.to_string(),
            locks: "none".to_string(),
            pressure,
            task: task.to_string(),
            nodes: 0,
            tokens_hr: 0,
        },
    )
    .await;
}
