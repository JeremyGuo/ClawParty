use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.28"
    }

    fn to_version(&self) -> &'static str {
        "0.29"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        backfill_cron_task_timezones(&workdir.join("cron").join("tasks.json"))
    }
}

fn backfill_cron_task_timezones(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let Some(tasks) = value.get_mut("tasks").and_then(Value::as_array_mut) else {
        return Ok(());
    };

    let mut changed = false;
    for task in tasks {
        let Some(task) = task.as_object_mut() else {
            continue;
        };
        if !task.contains_key("timezone") {
            task.insert("timezone".to_string(), json!("Asia/Shanghai"));
            changed = true;
        }
    }

    if changed {
        fs::write(path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}
