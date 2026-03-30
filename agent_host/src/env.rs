use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn load_dotenv_files(config_path: &Path) -> Result<Vec<PathBuf>> {
    let mut loaded = Vec::new();
    let mut seen = HashSet::new();

    let cwd = std::env::current_dir()?;
    let cwd_dotenv = cwd.join(".env");
    if seen.insert(cwd_dotenv.clone()) && cwd_dotenv.is_file() {
        dotenvy::from_path(&cwd_dotenv)?;
        loaded.push(cwd_dotenv);
    }

    if let Some(config_dir) = config_path.parent() {
        let config_dotenv = config_dir.join(".env");
        if seen.insert(config_dotenv.clone()) && config_dotenv.is_file() {
            dotenvy::from_path(&config_dotenv)?;
            loaded.push(config_dotenv);
        }
    }

    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::load_dotenv_files;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn loads_dotenv_from_config_directory() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("cfg");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join(".env"),
            "AGENT_HOST_TEST_ENV=loaded_from_dotenv\n",
        )
        .unwrap();
        fs::write(config_dir.join("config.json"), "{}").unwrap();

        // Safe here because this unit test is single-purpose and we clean up the key.
        unsafe {
            std::env::remove_var("AGENT_HOST_TEST_ENV");
        }
        let loaded = load_dotenv_files(&config_dir.join("config.json")).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            std::env::var("AGENT_HOST_TEST_ENV").unwrap(),
            "loaded_from_dotenv"
        );
        unsafe {
            std::env::remove_var("AGENT_HOST_TEST_ENV");
        }
    }
}
