use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.24"
    }

    fn to_version(&self) -> &'static str {
        "0.25"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        normalize_remote_workpaths_in_files(&workdir.join("conversations"), "conversation.json")?;
        normalize_remote_workpaths_in_files(&workdir.join("snapshots"), "snapshot.json")?;
        Ok(())
    }
}

fn normalize_remote_workpaths_in_files(root: &Path, file_name: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path().join(file_name);
        normalize_remote_workpaths_file_if_exists(&path)?;
    }

    Ok(())
}

fn normalize_remote_workpaths_file_if_exists(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    normalize_remote_workpaths(&mut value);
    let updated = serde_json::to_string_pretty(&value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    fs::write(path, updated).with_context(|| format!("failed to write {}", path.display()))
}

fn normalize_remote_workpaths(value: &mut Value) {
    let Some(settings) = value.get_mut("settings").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(workpaths) = settings
        .get_mut("remote_workpaths")
        .and_then(Value::as_array_mut)
    else {
        return;
    };

    let mut seen_hosts = HashSet::new();
    let mut normalized = Vec::new();
    for mut item in workpaths.drain(..).rev() {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        let Some(host) = object
            .get("host")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "local")
        else {
            continue;
        };
        if !seen_hosts.insert(host.to_string()) {
            continue;
        }
        object.insert("host".to_string(), Value::String(host.to_string()));
        for key in ["path", "description"] {
            if let Some(trimmed) = object
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
            {
                object.insert(key.to_string(), Value::String(trimmed));
            }
        }
        normalized.push(item);
    }
    normalized.reverse();
    *workpaths = normalized;
}

#[cfg(test)]
mod tests {
    use super::normalize_remote_workpaths;
    use serde_json::json;

    #[test]
    fn keeps_one_remote_workpath_per_host_with_last_entry_winning() {
        let mut value = json!({
            "settings": {
                "remote_workpaths": [
                    {"host": "wuwen-dev6", "path": "/old", "description": "old"},
                    {"host": "wuwen-dev3", "path": "/other", "description": "other"},
                    {"host": " wuwen-dev6 ", "path": " /new ", "description": " new "}
                ]
            }
        });

        normalize_remote_workpaths(&mut value);

        assert_eq!(
            value["settings"]["remote_workpaths"],
            json!([
                {"host": "wuwen-dev3", "path": "/other", "description": "other"},
                {"host": "wuwen-dev6", "path": "/new", "description": "new"}
            ])
        );
    }

    #[test]
    fn drops_empty_and_local_remote_workpaths() {
        let mut value = json!({
            "settings": {
                "remote_workpaths": [
                    {"host": "", "path": "/empty", "description": "empty"},
                    {"host": "local", "path": "/local", "description": "local"},
                    {"host": "remote", "path": "/remote", "description": "remote"}
                ]
            }
        });

        normalize_remote_workpaths(&mut value);

        assert_eq!(
            value["settings"]["remote_workpaths"],
            json!([
                {"host": "remote", "path": "/remote", "description": "remote"}
            ])
        );
    }
}
