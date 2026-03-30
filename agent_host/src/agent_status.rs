use agent_frame::TokenUsage;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedAgentKind {
    Background,
    Subagent,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedAgentState {
    Enqueued,
    Running,
    Completed,
    TimedOut,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagedAgentRecord {
    pub id: Uuid,
    pub kind: ManagedAgentKind,
    #[serde(default)]
    pub parent_agent_id: Option<Uuid>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    pub channel_id: String,
    pub model_key: String,
    pub state: ManagedAgentState,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub usage: TokenUsage,
}

pub struct AgentRegistry {
    records: BTreeMap<Uuid, ManagedAgentRecord>,
    state_path: Option<PathBuf>,
}

impl AgentRegistry {
    pub fn load_or_create(workdir: impl AsRef<Path>) -> Result<Self> {
        let state_path = workdir.as_ref().join("agent_registry.json");
        if state_path.exists() {
            let raw = fs::read_to_string(&state_path)
                .with_context(|| format!("failed to read {}", state_path.display()))?;
            let persisted: PersistedAgentRegistry =
                serde_json::from_str(&raw).context("failed to parse agent registry state")?;
            return Ok(Self {
                records: persisted.records,
                state_path: Some(state_path),
            });
        }

        let registry = Self {
            records: BTreeMap::new(),
            state_path: Some(state_path),
        };
        registry.persist()?;
        Ok(registry)
    }

    pub fn register(&mut self, record: ManagedAgentRecord) -> Result<()> {
        self.records.insert(record.id, record);
        self.persist()
    }

    pub fn mark_running(&mut self, id: Uuid, started_at: DateTime<Utc>) -> Result<()> {
        if let Some(record) = self.records.get_mut(&id) {
            record.state = ManagedAgentState::Running;
            record.started_at = Some(started_at);
            record.error = None;
        }
        self.persist()
    }

    pub fn mark_completed(
        &mut self,
        id: Uuid,
        finished_at: DateTime<Utc>,
        usage: TokenUsage,
    ) -> Result<()> {
        if let Some(record) = self.records.get_mut(&id) {
            record.state = ManagedAgentState::Completed;
            record.finished_at = Some(finished_at);
            record.error = None;
            record.usage = usage;
        }
        self.persist()
    }

    pub fn mark_failed(
        &mut self,
        id: Uuid,
        finished_at: DateTime<Utc>,
        usage: TokenUsage,
        error: String,
    ) -> Result<()> {
        if let Some(record) = self.records.get_mut(&id) {
            record.state = ManagedAgentState::Failed;
            record.finished_at = Some(finished_at);
            record.error = Some(error);
            record.usage = usage;
        }
        self.persist()
    }

    pub fn mark_timed_out(
        &mut self,
        id: Uuid,
        finished_at: DateTime<Utc>,
        usage: TokenUsage,
        error: String,
    ) -> Result<()> {
        if let Some(record) = self.records.get_mut(&id) {
            record.state = ManagedAgentState::TimedOut;
            record.finished_at = Some(finished_at);
            record.error = Some(error);
            record.usage = usage;
        }
        self.persist()
    }

    pub fn list_by_kind(&self, kind: ManagedAgentKind) -> Vec<ManagedAgentRecord> {
        self.records
            .values()
            .filter(|record| record.kind == kind)
            .cloned()
            .collect()
    }

    pub fn get(&self, id: Uuid) -> Option<ManagedAgentRecord> {
        self.records.get(&id).cloned()
    }

    pub fn has_active_children(&self, parent_agent_id: Uuid) -> bool {
        self.records.values().any(|record| {
            record.parent_agent_id == Some(parent_agent_id)
                && matches!(
                    record.state,
                    ManagedAgentState::Enqueued | ManagedAgentState::Running
                )
        })
    }

    fn persist(&self) -> Result<()> {
        let Some(state_path) = &self.state_path else {
            return Ok(());
        };
        let persisted = PersistedAgentRegistry {
            records: self.records.clone(),
        };
        let raw = serde_json::to_string_pretty(&persisted)
            .context("failed to serialize agent registry state")?;
        fs::write(state_path, raw)
            .with_context(|| format!("failed to write {}", state_path.display()))
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self {
            records: BTreeMap::new(),
            state_path: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct PersistedAgentRegistry {
    #[serde(default)]
    records: BTreeMap<Uuid, ManagedAgentRecord>,
}

#[cfg(test)]
mod tests {
    use super::{AgentRegistry, ManagedAgentKind, ManagedAgentRecord, ManagedAgentState};
    use agent_frame::TokenUsage;
    use chrono::Utc;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[test]
    fn agent_registry_persists_and_restores_records() {
        let temp_dir = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        let created_at = Utc::now();

        let mut registry = AgentRegistry::load_or_create(temp_dir.path()).unwrap();
        registry
            .register(ManagedAgentRecord {
                id,
                kind: ManagedAgentKind::Background,
                parent_agent_id: None,
                session_id: None,
                channel_id: "telegram-main".to_string(),
                model_key: "main".to_string(),
                state: ManagedAgentState::Enqueued,
                created_at,
                started_at: None,
                finished_at: None,
                error: None,
                usage: TokenUsage::default(),
            })
            .unwrap();
        registry.mark_running(id, created_at).unwrap();
        registry
            .mark_completed(
                id,
                created_at,
                TokenUsage {
                    llm_calls: 1,
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cache_hit_tokens: 0,
                    cache_miss_tokens: 10,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
            )
            .unwrap();

        let restored = AgentRegistry::load_or_create(temp_dir.path()).unwrap();
        let record = restored.get(id).unwrap();
        assert_eq!(record.state, ManagedAgentState::Completed);
        assert_eq!(record.channel_id, "telegram-main");
        assert_eq!(record.model_key, "main");
        assert_eq!(record.usage.total_tokens, 15);
    }

    #[test]
    fn agent_registry_detects_active_children() {
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let created_at = Utc::now();
        let mut registry = AgentRegistry::default();
        registry
            .register(ManagedAgentRecord {
                id: child_id,
                kind: ManagedAgentKind::Subagent,
                parent_agent_id: Some(parent_id),
                session_id: None,
                channel_id: "telegram-main".to_string(),
                model_key: "main".to_string(),
                state: ManagedAgentState::Running,
                created_at,
                started_at: Some(created_at),
                finished_at: None,
                error: None,
                usage: TokenUsage::default(),
            })
            .unwrap();
        assert!(registry.has_active_children(parent_id));
        registry
            .mark_completed(child_id, created_at, TokenUsage::default())
            .unwrap();
        assert!(!registry.has_active_children(parent_id));
    }

    #[test]
    fn agent_registry_persists_timed_out_state() {
        let temp_dir = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        let created_at = Utc::now();

        let mut registry = AgentRegistry::load_or_create(temp_dir.path()).unwrap();
        registry
            .register(ManagedAgentRecord {
                id,
                kind: ManagedAgentKind::Background,
                parent_agent_id: None,
                session_id: None,
                channel_id: "telegram-main".to_string(),
                model_key: "main".to_string(),
                state: ManagedAgentState::Running,
                created_at,
                started_at: Some(created_at),
                finished_at: None,
                error: None,
                usage: TokenUsage::default(),
            })
            .unwrap();
        registry
            .mark_timed_out(
                id,
                created_at,
                TokenUsage {
                    llm_calls: 1,
                    prompt_tokens: 12,
                    completion_tokens: 3,
                    total_tokens: 15,
                    cache_hit_tokens: 0,
                    cache_miss_tokens: 12,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                "timed out".to_string(),
            )
            .unwrap();

        let restored = AgentRegistry::load_or_create(temp_dir.path()).unwrap();
        let record = restored.get(id).unwrap();
        assert_eq!(record.state, ManagedAgentState::TimedOut);
        assert_eq!(record.usage.total_tokens, 15);
        assert_eq!(record.error.as_deref(), Some("timed out"));
    }
}
