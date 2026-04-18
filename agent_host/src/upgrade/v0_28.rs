use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.27"
    }

    fn to_version(&self) -> &'static str {
        "0.28"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        backfill_session_prompt_components(&workdir.join("sessions"), "session.json")?;
        backfill_snapshot_prompt_components(&workdir.join("snapshots"), "snapshot.json")?;
        Ok(())
    }
}

fn backfill_session_prompt_components(root: &Path, file_name: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path().join(file_name);
        if !path.is_file() {
            continue;
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        remove_obsolete_session_prompt_fields(object);
        let session_state = object.entry("session_state").or_insert_with(|| json!({}));
        let Some(session_state) = session_state.as_object_mut() else {
            continue;
        };
        remove_obsolete_prompt_hash_fields(session_state);
        session_state
            .entry("prompt_components")
            .or_insert_with(|| json!({}));

        fs::write(&path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

fn backfill_snapshot_prompt_components(root: &Path, file_name: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path().join(file_name);
        if !path.is_file() {
            continue;
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        let Some(session) = object.get_mut("session").and_then(Value::as_object_mut) else {
            continue;
        };
        remove_obsolete_session_prompt_fields(session);
        remove_obsolete_prompt_hash_fields(session);
        session
            .entry("prompt_components")
            .or_insert_with(|| json!({}));

        fs::write(&path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

fn remove_obsolete_session_prompt_fields(object: &mut serde_json::Map<String, Value>) {
    for key in [
        "seen_user_profile_version",
        "seen_identity_profile_version",
        "pending_user_profile_notice",
        "pending_identity_profile_notice",
        "seen_model_catalog_version",
        "pending_model_catalog_notice",
    ] {
        object.remove(key);
    }
}

fn remove_obsolete_prompt_hash_fields(object: &mut serde_json::Map<String, Value>) {
    object.remove("system_prompt_component_hashes");
    object.remove("pending_system_prompt_component_notices");
}
