use crate::sink::SinkTarget;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub enum MainAgent {
    Foreground(ForegroundAgent),
    Background(BackgroundAgent),
}

#[derive(Clone, Debug)]
pub struct ForegroundAgent {
    pub id: Uuid,
    pub session_id: Uuid,
    pub channel_id: String,
    pub system_prompt: String,
}

#[derive(Clone, Debug)]
pub struct BackgroundAgent {
    pub id: Uuid,
    pub sinks: Vec<SinkTarget>,
}

#[derive(Clone, Debug)]
pub struct SubAgentSpec {
    pub id: Uuid,
    pub parent_agent_id: Uuid,
    pub docker_image: Option<String>,
    pub can_spawn_sub_agents: bool,
}
