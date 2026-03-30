use crate::domain::{ChannelAddress, MessageRole, SessionMessage, StoredAttachment};
use agent_frame::ChatMessage;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub address: ChannelAddress,
    pub root_dir: PathBuf,
    pub attachments_dir: PathBuf,
    pub message_count: usize,
    pub agent_message_count: usize,
    pub agent_messages: Vec<ChatMessage>,
    pub last_agent_returned_at: Option<DateTime<Utc>>,
    pub last_compacted_at: Option<DateTime<Utc>>,
    pub turn_count: u64,
    pub last_compacted_turn_count: u64,
}

#[derive(Debug)]
struct Session {
    id: Uuid,
    agent_id: Uuid,
    address: ChannelAddress,
    root_dir: PathBuf,
    attachments_dir: PathBuf,
    history: Vec<SessionMessage>,
    agent_messages: Vec<ChatMessage>,
    last_agent_returned_at: Option<DateTime<Utc>>,
    last_compacted_at: Option<DateTime<Utc>>,
    turn_count: u64,
    last_compacted_turn_count: u64,
}

impl Session {
    fn state_path(&self) -> PathBuf {
        self.root_dir.join("session.json")
    }

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.id,
            agent_id: self.agent_id,
            address: self.address.clone(),
            root_dir: self.root_dir.clone(),
            attachments_dir: self.attachments_dir.clone(),
            message_count: self.history.len(),
            agent_message_count: self.agent_messages.len(),
            agent_messages: self.agent_messages.clone(),
            last_agent_returned_at: self.last_agent_returned_at,
            last_compacted_at: self.last_compacted_at,
            turn_count: self.turn_count,
            last_compacted_turn_count: self.last_compacted_turn_count,
        }
    }

    fn push_message(
        &mut self,
        role: MessageRole,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) {
        self.history.push(SessionMessage {
            role,
            text,
            attachments,
        });
    }

    fn persist(&self) -> Result<()> {
        let state = PersistedSession {
            id: self.id,
            agent_id: self.agent_id,
            address: self.address.clone(),
            history: self.history.clone(),
            agent_messages: self.agent_messages.clone(),
            last_agent_returned_at: self.last_agent_returned_at,
            last_compacted_at: self.last_compacted_at,
            turn_count: self.turn_count,
            last_compacted_turn_count: self.last_compacted_turn_count,
        };
        let raw =
            serde_json::to_string_pretty(&state).context("failed to serialize session state")?;
        fs::write(self.state_path(), raw)
            .with_context(|| format!("failed to write {}", self.state_path().display()))
    }

    fn from_persisted(root_dir: PathBuf, persisted: PersistedSession) -> Result<Self> {
        let attachments_dir = root_dir.join("attachments");
        fs::create_dir_all(&attachments_dir)
            .with_context(|| format!("failed to create {}", attachments_dir.display()))?;
        Ok(Self {
            id: persisted.id,
            agent_id: persisted.agent_id,
            address: persisted.address,
            root_dir,
            attachments_dir,
            history: persisted.history,
            agent_messages: persisted.agent_messages,
            last_agent_returned_at: persisted.last_agent_returned_at,
            last_compacted_at: persisted.last_compacted_at,
            turn_count: persisted.turn_count,
            last_compacted_turn_count: persisted.last_compacted_turn_count,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSession {
    id: Uuid,
    agent_id: Uuid,
    address: ChannelAddress,
    #[serde(default)]
    history: Vec<SessionMessage>,
    #[serde(default)]
    agent_messages: Vec<ChatMessage>,
    #[serde(default)]
    last_agent_returned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_compacted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    turn_count: u64,
    #[serde(default)]
    last_compacted_turn_count: u64,
}

pub struct SessionManager {
    sessions_root: PathBuf,
    foreground_sessions: HashMap<String, Session>,
}

impl SessionManager {
    pub fn new(workdir: impl AsRef<Path>) -> Result<Self> {
        let sessions_root = workdir.as_ref().join("sessions");
        fs::create_dir_all(&sessions_root)
            .with_context(|| format!("failed to create {}", sessions_root.display()))?;
        let foreground_sessions = load_persisted_sessions(&sessions_root)?;
        Ok(Self {
            sessions_root,
            foreground_sessions,
        })
    }

    pub fn ensure_foreground(&mut self, address: &ChannelAddress) -> Result<SessionSnapshot> {
        let key = address.session_key();
        if !self.foreground_sessions.contains_key(&key) {
            let session = self.create_session(address)?;
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_created",
                channel_id = %address.channel_id,
                conversation_id = %address.conversation_id,
                root_dir = %session.root_dir.display(),
                "created foreground session"
            );
            self.foreground_sessions.insert(key.clone(), session);
        }
        Ok(self
            .foreground_sessions
            .get(&key)
            .expect("foreground session inserted")
            .snapshot())
    }

    pub fn reset_foreground(&mut self, address: &ChannelAddress) -> Result<SessionSnapshot> {
        self.destroy_foreground(address)?;
        self.ensure_foreground(address)
    }

    pub fn destroy_foreground(&mut self, address: &ChannelAddress) -> Result<()> {
        let key = address.session_key();
        if let Some(session) = self.foreground_sessions.remove(&key) {
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_destroying",
                root_dir = %session.root_dir.display(),
                "destroying foreground session"
            );
            if session.root_dir.exists() {
                fs::remove_dir_all(&session.root_dir).with_context(|| {
                    format!(
                        "failed to remove session directory {}",
                        session.root_dir.display()
                    )
                })?;
            }
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_destroyed",
                "foreground session removed"
            );
        }
        Ok(())
    }

    pub fn append_user_message(
        &mut self,
        address: &ChannelAddress,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        self.append_message(address, MessageRole::User, text, attachments)
    }

    pub fn append_assistant_message(
        &mut self,
        address: &ChannelAddress,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        self.append_message(address, MessageRole::Assistant, text, attachments)
    }

    pub fn get_snapshot(&self, address: &ChannelAddress) -> Option<SessionSnapshot> {
        self.foreground_sessions
            .get(&address.session_key())
            .map(Session::snapshot)
    }

    pub fn list_foreground_snapshots(&self) -> Vec<SessionSnapshot> {
        self.foreground_sessions
            .values()
            .map(Session::snapshot)
            .collect()
    }

    pub fn update_agent_messages(
        &mut self,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.agent_messages = messages;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "agent_messages_updated",
            agent_message_count = session.agent_messages.len() as u64,
            "updated agent_frame message history"
        );
        session.persist()?;
        Ok(())
    }

    pub fn record_agent_turn(
        &mut self,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.agent_messages = messages;
        session.last_agent_returned_at = Some(Utc::now());
        session.turn_count = session.turn_count.saturating_add(1);
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "agent_turn_recorded",
            agent_message_count = session.agent_messages.len() as u64,
            turn_count = session.turn_count,
            "recorded successful agent turn"
        );
        session.persist()?;
        Ok(())
    }

    pub fn record_idle_compaction(
        &mut self,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.agent_messages = messages;
        session.last_compacted_at = Some(Utc::now());
        session.last_compacted_turn_count = session.turn_count;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "idle_context_compacted",
            agent_message_count = session.agent_messages.len() as u64,
            turn_count = session.turn_count,
            "persisted idle context compaction"
        );
        session.persist()?;
        Ok(())
    }

    fn append_message(
        &mut self,
        address: &ChannelAddress,
        role: MessageRole,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        let attachment_count = attachments.len();
        session.push_message(role, text, attachments);
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "message_appended",
            role = ?role,
            message_count = session.history.len() as u64,
            attachment_count = attachment_count as u64,
            "appended message to session history"
        );
        session.persist()?;
        Ok(())
    }

    fn create_session(&self, address: &ChannelAddress) -> Result<Session> {
        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let root_dir = self.sessions_root.join(session_id.to_string());
        let attachments_dir = root_dir.join("attachments");
        fs::create_dir_all(&attachments_dir).with_context(|| {
            format!(
                "failed to create session attachment directory {}",
                attachments_dir.display()
            )
        })?;
        let session = Session {
            id: session_id,
            agent_id,
            address: address.clone(),
            root_dir,
            attachments_dir,
            history: Vec::new(),
            agent_messages: Vec::new(),
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
        };
        session.persist()?;
        Ok(session)
    }
}

fn load_persisted_sessions(sessions_root: &Path) -> Result<HashMap<String, Session>> {
    let mut sessions = HashMap::new();
    for entry in fs::read_dir(sessions_root)
        .with_context(|| format!("failed to read {}", sessions_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_path = path.join("session.json");
        if !state_path.exists() {
            continue;
        }
        match load_single_session(&path, &state_path) {
            Ok(session) => {
                let key = session.address.session_key();
                info!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "session_restored",
                    channel_id = %session.address.channel_id,
                    conversation_id = %session.address.conversation_id,
                    root_dir = %session.root_dir.display(),
                    "restored persisted foreground session"
                );
                sessions.insert(key, session);
            }
            Err(error) => {
                warn!(
                    log_stream = "session",
                    kind = "session_restore_failed",
                    root_dir = %path.display(),
                    error = %format!("{error:#}"),
                    "failed to restore persisted session; skipping"
                );
            }
        }
    }
    Ok(sessions)
}

fn load_single_session(root_dir: &Path, state_path: &Path) -> Result<Session> {
    let raw = fs::read_to_string(state_path)
        .with_context(|| format!("failed to read {}", state_path.display()))?;
    let persisted: PersistedSession =
        serde_json::from_str(&raw).context("failed to parse session state")?;
    Session::from_persisted(root_dir.to_path_buf(), persisted)
}
