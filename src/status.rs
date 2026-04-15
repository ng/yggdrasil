use std::path::Path;
use serde::Serialize;
use uuid::Uuid;

const STATUS_DIR: &str = "/tmp/ygg";

/// Agent state written to /tmp/ygg/agent-{session_id}.json for the status bar script.
#[derive(Debug, Serialize)]
pub struct AgentStatus {
    pub state: String,
    pub locks: String,
    pub pressure: u32,
    pub task: String,
    pub nodes: u32,
    pub tokens_hr: u64,
}

/// Write agent status to the shared file (atomic rename).
pub async fn write_status(session_id: &str, status: &AgentStatus) -> Result<(), crate::YggError> {
    let dir = Path::new(STATUS_DIR);
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| crate::YggError::Config(format!("create status dir: {e}")))?;

    let target = dir.join(format!("agent-{session_id}.json"));
    let tmp = dir.join(format!(".agent-{session_id}.json.tmp"));

    let data = serde_json::to_string(status)
        .map_err(|e| crate::YggError::Config(format!("serialize status: {e}")))?;

    tokio::fs::write(&tmp, &data)
        .await
        .map_err(|e| crate::YggError::Config(format!("write status tmp: {e}")))?;

    tokio::fs::rename(&tmp, &target)
        .await
        .map_err(|e| crate::YggError::Config(format!("rename status: {e}")))?;

    Ok(())
}

/// Clean up status file when agent shuts down.
pub async fn remove_status(session_id: &str) {
    let path = Path::new(STATUS_DIR).join(format!("agent-{session_id}.json"));
    let _ = tokio::fs::remove_file(path).await;
}

/// Generate a unique session ID for this agent instance.
pub fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}
