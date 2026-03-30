use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const USER_TEMPLATE: &str = r#"---
name: workspace-user
description: Persistent notes about the human behind this workspace.
experience:
  - Product thinking
  - Fast iteration
  - Prefers direct technical communication
---

# User

Write durable facts about the user here.

Suggested topics:
- Background and experience
- Current goals
- Communication preferences
- Things the agent should remember across sessions
"#;

const IDENTITY_TEMPLATE: &str = r#"# Your name - What should they call you?
# Your nature - What kind of creature are you? AI assistant is fine, but maybe you are something weirder.
# Your vibe - Formal, casual, sharp, warm, or something else?
# Your emoji - Everyone needs a signature.
# Your mission - How should you help this user in this workspace?
"#;

const AGENTS_TEMPLATE: &str = "";

const SKILL_CREATOR_TEMPLATE: &str = r#"---
name: skill-creator
description: Create or update a skill folder with a valid SKILL.md frontmatter/body format and keep the instructions concise.
---

# Skill Creator

Use this skill when creating or revising a skill under the local runtime skills directory.

Requirements:
- Every skill must contain a SKILL.md file.
- SKILL.md must begin with YAML frontmatter containing:
  - name
  - description
- Keep the body concise and procedural.
- Prefer putting durable workflow instructions in SKILL.md.
- Do not create extra documentation files unless they are directly needed by the skill.

Recommended layout:
- SKILL.md
- references/ only if extra material is truly needed
- scripts/ only when deterministic execution is important
"#;

#[derive(Clone, Debug)]
pub struct AgentWorkspace {
    pub root_dir: PathBuf,
    pub agent_dir: PathBuf,
    pub rundir: PathBuf,
    pub projects_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub skill_creator_dir: PathBuf,
    pub user_md_path: PathBuf,
    pub identity_md_path: PathBuf,
    pub agents_md_path: PathBuf,
    pub user_profile_markdown: String,
    pub raw_identity_markdown: String,
    pub identity_prompt: String,
    pub agents_markdown: String,
}

impl AgentWorkspace {
    pub fn initialize(workdir: impl AsRef<Path>) -> Result<Self> {
        let root_dir = workdir.as_ref().to_path_buf();
        let agent_dir = root_dir.join("agent");
        let rundir = root_dir.join("rundir");
        let projects_dir = rundir.join("projects");
        let tmp_dir = rundir.join("tmp");
        let skills_dir = rundir.join(".skills");
        let skill_creator_dir = skills_dir.join("skill-creator");
        fs::create_dir_all(&agent_dir)
            .with_context(|| format!("failed to create {}", agent_dir.display()))?;
        fs::create_dir_all(&rundir)
            .with_context(|| format!("failed to create {}", rundir.display()))?;
        fs::create_dir_all(&projects_dir)
            .with_context(|| format!("failed to create {}", projects_dir.display()))?;
        fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("failed to create {}", tmp_dir.display()))?;
        fs::create_dir_all(&skill_creator_dir)
            .with_context(|| format!("failed to create {}", skill_creator_dir.display()))?;

        let user_md_path = agent_dir.join("USER.md");
        let identity_md_path = agent_dir.join("IDENTITY.md");
        let agents_md_path = rundir.join("AGENTS.md");
        let skill_creator_md_path = skill_creator_dir.join("SKILL.md");

        ensure_seed_file(&user_md_path, USER_TEMPLATE)?;
        ensure_seed_file(&identity_md_path, IDENTITY_TEMPLATE)?;
        ensure_seed_file(&agents_md_path, AGENTS_TEMPLATE)?;
        ensure_seed_file(&skill_creator_md_path, SKILL_CREATOR_TEMPLATE)?;

        let user_profile_markdown = fs::read_to_string(&user_md_path)
            .with_context(|| format!("failed to read {}", user_md_path.display()))?;
        let raw_identity_markdown = fs::read_to_string(&identity_md_path)
            .with_context(|| format!("failed to read {}", identity_md_path.display()))?;
        let identity_prompt = render_identity_prompt(&raw_identity_markdown);
        let agents_markdown = fs::read_to_string(&agents_md_path)
            .with_context(|| format!("failed to read {}", agents_md_path.display()))?;

        Ok(Self {
            root_dir,
            agent_dir,
            rundir,
            projects_dir,
            tmp_dir,
            skills_dir,
            skill_creator_dir,
            user_md_path,
            identity_md_path,
            agents_md_path,
            user_profile_markdown,
            raw_identity_markdown,
            identity_prompt,
            agents_markdown,
        })
    }
}

fn ensure_seed_file(path: &Path, template: &str) -> Result<()> {
    if !path.exists() {
        fs::write(path, template)
            .with_context(|| format!("failed to write template {}", path.display()))?;
        return Ok(());
    }

    let existing = fs::read_to_string(path)
        .with_context(|| format!("failed to read existing file {}", path.display()))?;
    if existing.trim().is_empty() {
        fs::write(path, template)
            .with_context(|| format!("failed to write template {}", path.display()))?;
    }
    Ok(())
}

fn render_identity_prompt(markdown: &str) -> String {
    markdown
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                None
            } else if trimmed.is_empty() {
                Some(String::new())
            } else {
                Some(line.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::AgentWorkspace;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn initializes_workspace_templates_and_preserves_existing_identity() {
        let temp_dir = TempDir::new().unwrap();
        let identity_path = temp_dir.path().join("agent").join("IDENTITY.md");
        fs::create_dir_all(identity_path.parent().unwrap()).unwrap();
        fs::write(&identity_path, "# Existing identity\n# Keep this\n").unwrap();

        let workspace = AgentWorkspace::initialize(temp_dir.path()).unwrap();
        assert!(workspace.user_md_path.exists());
        assert!(workspace.identity_md_path.exists());
        assert!(workspace.agents_md_path.exists());
        assert!(workspace.projects_dir.exists());
        assert!(workspace.tmp_dir.exists());
        assert!(
            workspace
                .raw_identity_markdown
                .starts_with("# Existing identity")
        );
        assert!(workspace.identity_prompt.is_empty());
        assert!(workspace.user_profile_markdown.contains("experience:"));
        assert!(workspace.skill_creator_dir.join("SKILL.md").exists());
    }

    #[test]
    fn identity_prompt_ignores_commented_lines() {
        let temp_dir = TempDir::new().unwrap();
        let identity_path = temp_dir.path().join("agent").join("IDENTITY.md");
        fs::create_dir_all(identity_path.parent().unwrap()).unwrap();
        fs::write(
            &identity_path,
            "# Commented heading\nYou are Claw.\n# Hidden note\nWarm and direct.\n",
        )
        .unwrap();

        let workspace = AgentWorkspace::initialize(temp_dir.path()).unwrap();
        assert_eq!(workspace.identity_prompt, "You are Claw.\nWarm and direct.");
        assert!(workspace.projects_dir.ends_with("rundir/projects"));
        assert!(workspace.tmp_dir.ends_with("rundir/tmp"));
    }
}
