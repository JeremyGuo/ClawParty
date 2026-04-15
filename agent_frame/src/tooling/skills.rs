use super::args::string_arg;
use super::{InterruptSignal, Tool};
use crate::skills::{
    SkillMetadata, build_skill_index, load_skill_by_name, validate_skill_markdown,
};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

fn validate_skill_name_component(skill_name: &str) -> Result<String> {
    let trimmed = skill_name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("skill_name must be a non-empty string"));
    }
    if trimmed == "." || trimmed == ".." || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(anyhow!(
            "skill_name must be a single path component without path separators"
        ));
    }
    Ok(trimmed.to_string())
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", source.display()))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", source_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn writable_skill_root(skill_roots: &[PathBuf]) -> Result<PathBuf> {
    skill_roots
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no writable skills directory is configured"))
}

fn persist_staged_skill_directory(
    workspace_root: &Path,
    skill_roots: &[PathBuf],
    skill_name: &str,
    require_existing: bool,
) -> Result<Value> {
    let skill_name = validate_skill_name_component(skill_name)?;
    let staged_dir = workspace_root.join(".skills").join(&skill_name);
    let staged_skill_md = staged_dir.join("SKILL.md");
    if !staged_skill_md.is_file() {
        return Err(anyhow!(
            "staged skill '{}' must exist at {}",
            skill_name,
            staged_skill_md.display()
        ));
    }
    let content = fs::read_to_string(&staged_skill_md)
        .with_context(|| format!("failed to read {}", staged_skill_md.display()))?;
    let (declared_name, description) = validate_skill_markdown(&content)?;
    if declared_name != skill_name {
        return Err(anyhow!(
            "SKILL.md frontmatter name '{}' must match skill_name '{}'",
            declared_name,
            skill_name
        ));
    }

    let skill_root = writable_skill_root(skill_roots)?;
    fs::create_dir_all(&skill_root)
        .with_context(|| format!("failed to create {}", skill_root.display()))?;
    let target_dir = skill_root.join(&skill_name);
    let target_exists = target_dir.exists();
    if require_existing && !target_exists {
        return Err(anyhow!(
            "cannot update unknown skill '{}'; create it first",
            skill_name
        ));
    }
    if !require_existing && target_exists {
        return Err(anyhow!(
            "skill '{}' already exists; use skill_update instead",
            skill_name
        ));
    }

    let temp_dir = skill_root.join(format!(".tmp-skill-{}-{}", skill_name, Uuid::new_v4()));
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)
            .with_context(|| format!("failed to remove {}", temp_dir.display()))?;
    }
    copy_dir_recursive(&staged_dir, &temp_dir)?;
    if target_exists {
        fs::remove_dir_all(&target_dir)
            .with_context(|| format!("failed to replace {}", target_dir.display()))?;
    }
    fs::rename(&temp_dir, &target_dir).with_context(|| {
        format!(
            "failed to move {} into {}",
            temp_dir.display(),
            target_dir.display()
        )
    })?;

    Ok(json!({
        "name": declared_name,
        "description": description,
        "persisted": true,
        "created": !require_existing,
        "updated": require_existing
    }))
}

pub(super) fn skill_load_tool(
    skills: &[SkillMetadata],
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Result<Tool> {
    let skill_index = build_skill_index(skills)?;
    let available_skills = skill_index.keys().cloned().collect::<Vec<_>>();
    Ok(Tool::new(
        "skill_load",
        "Load the SKILL.md instructions for a named skill. Use exact skill names from the preloaded metadata.",
        json!({
            "type": "object",
            "properties": {
                "skill_name": {"type": "string", "enum": available_skills}
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let skill_name = string_arg(arguments, "skill_name")?;
            let (skill, content) = load_skill_by_name(&skill_index, &skill_name)?;
            Ok(json!({
                "name": skill.name,
                "description": skill.description,
                "content": content
            }))
        },
    ))
}

pub(super) fn skill_create_tool(workspace_root: PathBuf, skill_roots: Vec<PathBuf>) -> Tool {
    Tool::new(
        "skill_create",
        "Persist a staged skill directory from .skills/<skill_name>/ in the current workspace into the runtime skills store as a new skill. Validate SKILL.md and fail with the validation reason if invalid.",
        json!({
            "type": "object",
            "properties": {
                "skill_name": {"type": "string"}
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let skill_name = string_arg(arguments, "skill_name")?;
            persist_staged_skill_directory(&workspace_root, &skill_roots, &skill_name, false)
        },
    )
}

pub(super) fn skill_update_tool(workspace_root: PathBuf, skill_roots: Vec<PathBuf>) -> Tool {
    Tool::new(
        "skill_update",
        "Persist a staged skill directory from .skills/<skill_name>/ in the current workspace into the runtime skills store as an update to an existing skill. Validate SKILL.md and fail with the validation reason if invalid.",
        json!({
            "type": "object",
            "properties": {
                "skill_name": {"type": "string"}
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let skill_name = string_arg(arguments, "skill_name")?;
            persist_staged_skill_directory(&workspace_root, &skill_roots, &skill_name, true)
        },
    )
}
