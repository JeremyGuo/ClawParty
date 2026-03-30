use agent_frame::ChatMessage;
use agent_host::agent_status::{
    AgentRegistry, ManagedAgentKind, ManagedAgentRecord, ManagedAgentState,
};
use agent_host::bootstrap::AgentWorkspace;
use agent_host::channel::{LocalFileAttachmentSource, PendingAttachment};
use agent_host::domain::{AttachmentKind, ChannelAddress};
use agent_host::prompt::greeting_for_language;
use agent_host::session::SessionManager;
use chrono::Utc;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn session_reset_removes_attachment_directory() {
    let temp_dir = TempDir::new().unwrap();
    let source_file = temp_dir.path().join("note.txt");
    std::fs::write(&source_file, "hello").unwrap();

    let address = ChannelAddress {
        channel_id: "cli".to_string(),
        conversation_id: "stdin".to_string(),
        user_id: Some("user-1".to_string()),
        display_name: Some("CLI".to_string()),
    };

    let mut manager = SessionManager::new(temp_dir.path()).unwrap();
    let session = manager.ensure_foreground(&address).unwrap();
    let attachment = PendingAttachment::new(
        AttachmentKind::File,
        Some("note.txt".to_string()),
        Some("text/plain".to_string()),
        None,
        Arc::new(LocalFileAttachmentSource::new(&source_file)),
    );
    let stored = attachment
        .materialize(&session.attachments_dir)
        .await
        .unwrap();
    assert!(stored.path.exists());
    let original_root = session.root_dir.clone();

    let new_session = manager.reset_foreground(&address).unwrap();
    assert!(!original_root.exists());
    assert!(new_session.root_dir.exists());
}

#[test]
fn workspace_bootstrap_creates_agent_files() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = AgentWorkspace::initialize(temp_dir.path()).unwrap();
    assert!(workspace.user_md_path.exists());
    assert!(workspace.identity_md_path.exists());
    assert!(workspace.agents_md_path.exists());
    assert!(workspace.projects_dir.exists());
    assert!(workspace.skill_creator_dir.join("SKILL.md").exists());
    assert!(workspace.identity_prompt.is_empty());
}

#[test]
fn greeting_defaults_to_language_specific_value() {
    assert_eq!(greeting_for_language("zh-CN"), "你好");
    assert_eq!(greeting_for_language("en"), "Hello");
}

#[test]
fn session_manager_restores_persisted_foreground_session_after_restart() {
    let temp_dir = TempDir::new().unwrap();
    let address = ChannelAddress {
        channel_id: "telegram-main".to_string(),
        conversation_id: "1717801091".to_string(),
        user_id: Some("user-1".to_string()),
        display_name: Some("Telegram User".to_string()),
    };

    let original = {
        let mut manager = SessionManager::new(temp_dir.path()).unwrap();
        let _session = manager.ensure_foreground(&address).unwrap();
        manager
            .append_user_message(&address, Some("hello".to_string()), Vec::new())
            .unwrap();
        manager
            .record_agent_turn(&address, vec![ChatMessage::text("assistant", "persist me")])
            .unwrap();
        manager.get_snapshot(&address).unwrap()
    };

    let mut restored_manager = SessionManager::new(temp_dir.path()).unwrap();
    let restored = restored_manager.ensure_foreground(&address).unwrap();
    assert_eq!(restored.id, original.id);
    assert_eq!(restored.agent_id, original.agent_id);
    assert_eq!(restored.message_count, 1);
    assert_eq!(restored.agent_message_count, 1);
    assert_eq!(restored.turn_count, 1);
    assert!(restored.last_agent_returned_at.is_some());
    assert_eq!(
        restored.agent_messages,
        vec![ChatMessage::text("assistant", "persist me")]
    );
}

#[test]
fn session_manager_persists_idle_compaction_metadata_after_restart() {
    let temp_dir = TempDir::new().unwrap();
    let address = ChannelAddress {
        channel_id: "telegram-main".to_string(),
        conversation_id: "1717801091".to_string(),
        user_id: Some("user-1".to_string()),
        display_name: Some("Telegram User".to_string()),
    };

    let original = {
        let mut manager = SessionManager::new(temp_dir.path()).unwrap();
        let _session = manager.ensure_foreground(&address).unwrap();
        manager
            .record_agent_turn(
                &address,
                vec![ChatMessage::text("assistant", "before compact")],
            )
            .unwrap();
        manager
            .record_idle_compaction(
                &address,
                vec![ChatMessage::text(
                    "assistant",
                    "[AgentFrame Context Compression]\n\ncompressed",
                )],
            )
            .unwrap();
        manager.get_snapshot(&address).unwrap()
    };

    assert_eq!(original.turn_count, 1);
    assert_eq!(original.last_compacted_turn_count, 1);
    assert!(original.last_agent_returned_at.is_some());
    assert!(original.last_compacted_at.is_some());

    let restored_manager = SessionManager::new(temp_dir.path()).unwrap();
    let restored = restored_manager.get_snapshot(&address).unwrap();
    assert_eq!(restored.id, original.id);
    assert_eq!(restored.turn_count, 1);
    assert_eq!(restored.last_compacted_turn_count, 1);
    assert!(restored.last_agent_returned_at.is_some());
    assert!(restored.last_compacted_at.is_some());
    assert_eq!(
        restored.agent_messages,
        vec![ChatMessage::text(
            "assistant",
            "[AgentFrame Context Compression]\n\ncompressed",
        )]
    );
}

#[test]
fn agent_registry_restores_background_agent_history_after_restart() {
    let temp_dir = TempDir::new().unwrap();
    let agent_id = Uuid::new_v4();
    let created_at = Utc::now();

    let mut registry = AgentRegistry::load_or_create(temp_dir.path()).unwrap();
    registry
        .register(ManagedAgentRecord {
            id: agent_id,
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
            usage: agent_frame::TokenUsage::default(),
        })
        .unwrap();
    registry.mark_running(agent_id, created_at).unwrap();
    registry
        .mark_failed(
            agent_id,
            created_at,
            agent_frame::TokenUsage {
                llm_calls: 1,
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_hit_tokens: 2,
                cache_miss_tokens: 8,
                cache_read_tokens: 2,
                cache_write_tokens: 1,
            },
            "boom".to_string(),
        )
        .unwrap();

    let restored = AgentRegistry::load_or_create(temp_dir.path()).unwrap();
    let record = restored.get(agent_id).unwrap();
    assert_eq!(record.kind, ManagedAgentKind::Background);
    assert_eq!(record.state, ManagedAgentState::Failed);
    assert_eq!(record.channel_id, "telegram-main");
    assert_eq!(record.model_key, "main");
    assert_eq!(record.usage.total_tokens, 15);
    assert_eq!(record.error.as_deref(), Some("boom"));
}
