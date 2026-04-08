use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.7"
    }

    fn to_version(&self) -> &'static str {
        "0.8"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        let runtime_root = workdir.join("agent").join("runtime");
        if !runtime_root.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&runtime_root)
            .with_context(|| format!("failed to read {}", runtime_root.display()))?
        {
            let workspace_runtime = entry?.path();
            let legacy_processes_dir = workspace_runtime.join("agent_frame").join("processes");
            if legacy_processes_dir.exists() {
                fs::remove_dir_all(&legacy_processes_dir).with_context(|| {
                    format!("failed to remove {}", legacy_processes_dir.display())
                })?;
            }
        }
        Ok(())
    }
}
