use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCleanupTarget {
    pub agent_id: Uuid,
    pub agent_name: String,
}
