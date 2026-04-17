//! Hook-driven agent state updates.
//!
//! Called from PreToolUse to record what the agent is doing right now so the
//! dashboard doesn't forever read "idle". Unconditional overwrite: hooks can't
//! guess the current state.

use crate::models::agent::{AgentRepo, AgentState};
use tracing::{debug, warn};

pub async fn set_tool(
    pool: &sqlx::PgPool,
    agent_name: &str,
    tool: &str,
) -> Result<(), anyhow::Error> {
    let repo = AgentRepo::new(pool);
    let Some(agent) = repo.get_by_name(agent_name).await? else {
        debug!("agent set-tool: '{agent_name}' not registered — skipping");
        return Ok(());
    };
    if let Err(e) = repo.force_state(agent.agent_id, AgentState::WaitingTool, Some(tool)).await {
        warn!("agent set-tool: force_state failed: {e}");
    }
    Ok(())
}
