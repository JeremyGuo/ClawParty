use std::path::{Component, Path, PathBuf};

#[cfg(not(test))]
use std::{process::Command, time::Duration};

use serde_json::{json, Map, Value};

use super::super::{
    schema::{file_tool_schema, properties},
    BaseTool, ToolBackend, ToolCallContext, ToolConcurrency, ToolDefinition, ToolExecutionMode,
    ToolRemoteMode,
};
use crate::session_actor::{
    tool_binary::ensure_tool_binary,
    tool_runtime::{
        bool_arg_with_default, clamp_tool_output_chars, run_remote_command_with_stdin, shell_quote,
        string_arg, string_arg_with_default, truncate_tool_text, usize_arg_with_default,
        ExecutionTarget, LocalToolError, ToolExecutionContext,
    },
    ToolResultContent,
};

const FS_TOOL_NAME: &str = "stellaclaw-fs-tool";
const REMOTE_SAFE_PATH_PREFIX: &str =
    "PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:$PATH}; export PATH;";

pub(crate) struct ApplyPatchTool {
    remote_mode: ToolRemoteMode,
}

impl ApplyPatchTool {
    pub(crate) fn new(remote_mode: &ToolRemoteMode) -> Self {
        Self {
            remote_mode: remote_mode.clone(),
        }
    }

    pub(crate) fn call_with_context(
        &self,
        args: Value,
        context: &ToolExecutionContext<'_>,
    ) -> Result<ToolResultContent, LocalToolError> {
        let arguments = object_arguments(args)?;
        self.execute(&arguments, context)
            .map(ToolResultContent::from_tool_value)
    }

    fn execute(
        &self,
        arguments: &Map<String, Value>,
        context: &ToolExecutionContext<'_>,
    ) -> Result<Value, LocalToolError> {
        match self.execution_target(arguments, context)? {
            ExecutionTarget::Local => {
                let local_arguments =
                    normalize_local_patch_arguments(arguments, context.workspace_root)?;
                fs_tool_local(&local_arguments, context)
            }
            ExecutionTarget::RemoteSsh { host, cwd } => {
                fs_tool_remote(arguments, context, &host, cwd.as_deref())
            }
        }
    }

    fn execution_target(
        &self,
        arguments: &Map<String, Value>,
        context: &ToolExecutionContext<'_>,
    ) -> Result<ExecutionTarget, LocalToolError> {
        if matches!(
            context.remote_mode,
            crate::session_actor::ToolRemoteMode::FixedSsh { .. }
        ) {
            let patch = string_arg(arguments, "patch")?;
            let format = patch_format(arguments, &patch)?;
            if classify_patch_target_paths(format, &patch, context.workspace_root)?
                == PatchTargetPaths::LocalSpecial
            {
                return Ok(ExecutionTarget::Local);
            }
        }
        context.execution_target(arguments)
    }

    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "apply_patch",
            "Apply a patch inside the workspace. Patch file paths must be workspace-relative paths, or remote-cwd-relative paths when remote execution is selected. Absolute paths under the active workspace/cwd are normalized to relative paths before applying; absolute paths outside it are rejected. Supports format=auto, format=freeform, format=codex, or format=unified. Freeform/codex format uses *** Begin Patch / *** End Patch sections. Unified format is passed to git apply; non-empty stdout/stderr are returned and capped by max_output_chars.",
            file_tool_schema(
                properties([
                    ("patch", json!({"type": "string"})),
                    (
                        "format",
                        json!({"type": "string", "enum": ["auto", "freeform", "codex", "unified"]}),
                    ),
                    ("strip", json!({"type": "integer"})),
                    ("reverse", json!({"type": "boolean"})),
                    ("check", json!({"type": "boolean"})),
                    (
                        "max_output_chars",
                        json!({"type": "integer", "minimum": 0, "maximum": 1000}),
                    ),
                ]),
                &["patch"],
                &self.remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial)
    }
}

impl BaseTool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        self.tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        self.call_with_context(args, &ctx.execution)
    }
}

fn object_arguments(args: Value) -> Result<Map<String, Value>, LocalToolError> {
    let Value::Object(arguments) = args else {
        return Err(LocalToolError::InvalidArguments(
            "tool arguments must be a JSON object".to_string(),
        ));
    };
    Ok(arguments)
}

fn normalize_local_patch_arguments(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
) -> Result<Map<String, Value>, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let format = patch_format(arguments, &patch)?;
    let normalized_patch = match format {
        PatchFormat::Freeform | PatchFormat::Codex => patch.clone(),
        PatchFormat::Unified => normalize_local_unified_patch_paths(&patch, workspace_root)?,
    };
    if normalized_patch == patch {
        return Ok(arguments.clone());
    }
    let mut arguments = arguments.clone();
    arguments.insert("patch".to_string(), Value::String(normalized_patch));
    Ok(arguments)
}

fn fs_tool_local(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let format = patch_format(arguments, &patch)?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let max_output_chars =
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?);

    #[cfg(test)]
    return patch_test::apply_local_for_test(
        arguments,
        context.workspace_root,
        format,
        check,
        max_output_chars,
    );

    #[cfg(not(test))]
    {
        use crate::session_actor::tool_runtime::run_command_with_timeout;

        let strip = usize_arg_with_default(arguments, "strip", 0)?;
        let reverse = bool_arg_with_default(arguments, "reverse", false)?;
        let binary = ensure_fs_tool_local(context)?;
        let mut command = Command::new(&binary);
        command
            .arg("apply-patch")
            .arg("--workspace")
            .arg(context.workspace_root)
            .arg("--format")
            .arg(format.cli_name())
            .arg("--max-output-chars")
            .arg(max_output_chars.to_string());
        if check {
            command.arg("--check");
        }
        if reverse {
            command.arg("--reverse");
        }
        if strip != 0 {
            command.arg("--strip").arg(strip.to_string());
        }

        let output = run_command_with_timeout(
            command,
            Duration::from_secs(300),
            Some(patch.as_bytes()),
            FS_TOOL_NAME,
        )?;
        if let Some(result) = parse_fs_tool_json(&output, None) {
            return Ok(mark_patch_result_format(result, format));
        }
        Ok(patch_result(output, None, max_output_chars))
    }
}

fn fs_tool_remote(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let format = patch_format(arguments, &patch)?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let max_output_chars =
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?);

    let remote_binary = ensure_fs_tool_remote(context, host)?;
    let mut args = vec![
        remote_binary,
        "apply-patch".to_string(),
        "--workspace".to_string(),
        ".".to_string(),
        "--format".to_string(),
        format.cli_name().to_string(),
        "--max-output-chars".to_string(),
        max_output_chars.to_string(),
    ];
    if check {
        args.push("--check".to_string());
    }
    if reverse {
        args.push("--reverse".to_string());
    }
    if strip != 0 {
        args.push("--strip".to_string());
        args.push(strip.to_string());
    }

    let remote_command = args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let remote_command = match cwd {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), remote_command),
        None => remote_command,
    };
    let remote_command = remote_shell_command(&remote_command);
    let output = run_remote_command_with_stdin(host, &remote_command, patch.as_bytes())?;
    if let Some(result) = parse_fs_tool_json(&output, Some(host)) {
        return Ok(mark_patch_result_format(result, format));
    }
    Ok(patch_result(output, Some(host), max_output_chars))
}

fn normalize_local_unified_patch_paths(
    patch: &str,
    workspace_root: &Path,
) -> Result<String, LocalToolError> {
    let mut changed = false;
    let mut output = String::with_capacity(patch.len());
    for segment in patch.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        let normalized = if let Some(rest) = line.strip_prefix("--- ") {
            normalize_local_unified_file_header("--- ", rest, workspace_root, &mut changed)?
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            normalize_local_unified_file_header("+++ ", rest, workspace_root, &mut changed)?
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            normalize_local_diff_git_header(rest, workspace_root, &mut changed)?
        } else {
            line.to_string()
        };
        output.push_str(&normalized);
        output.push_str(newline);
    }
    Ok(output)
}

fn normalize_local_unified_file_header(
    prefix: &str,
    rest: &str,
    workspace_root: &Path,
    changed: &mut bool,
) -> Result<String, LocalToolError> {
    let (path, suffix) = split_unified_header_path(rest);
    let normalized = normalize_local_unified_path_token(path, workspace_root)?;
    if normalized != path {
        *changed = true;
    }
    Ok(format!("{prefix}{normalized}{suffix}"))
}

fn normalize_local_diff_git_header(
    rest: &str,
    workspace_root: &Path,
    changed: &mut bool,
) -> Result<String, LocalToolError> {
    let mut parts = rest.split_whitespace();
    let Some(old_path) = parts.next() else {
        return Ok("diff --git ".to_string());
    };
    let Some(new_path) = parts.next() else {
        return Ok(format!("diff --git {rest}"));
    };
    if parts.next().is_some() {
        return Ok(format!("diff --git {rest}"));
    }
    let normalized_old = normalize_local_unified_path_token(old_path, workspace_root)?;
    let normalized_new = normalize_local_unified_path_token(new_path, workspace_root)?;
    if normalized_old != old_path || normalized_new != new_path {
        *changed = true;
    }
    Ok(format!("diff --git {normalized_old} {normalized_new}"))
}

fn split_unified_header_path(rest: &str) -> (&str, &str) {
    if let Some(index) = rest.find('\t') {
        return rest.split_at(index);
    }
    if rest.starts_with('/') {
        if let Some(index) = rest.find(char::is_whitespace) {
            return rest.split_at(index);
        }
    }
    (rest, "")
}

fn normalize_local_unified_path_token(
    path: &str,
    workspace_root: &Path,
) -> Result<String, LocalToolError> {
    if path == "/dev/null" || !Path::new(path).is_absolute() {
        return Ok(path.to_string());
    }
    let path_obj = Path::new(path);
    if let Ok(relative) = path_obj.strip_prefix(workspace_root) {
        if relative.as_os_str().is_empty() {
            return Err(LocalToolError::InvalidArguments(format!(
                "unified patch path {path:?} points at the local workspace root; use a file path"
            )));
        }
        return Ok(relative.display().to_string());
    }
    if let Some(relative) = local_special_relative_path(path_obj, workspace_root) {
        return Ok(relative.display().to_string());
    }
    Err(LocalToolError::InvalidArguments(format!(
        "unified patch path {path:?} is absolute and outside the local workspace {}; use a relative patch path",
        workspace_root.display()
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchFormat {
    Freeform,
    Codex,
    Unified,
}

impl PatchFormat {
    fn cli_name(self) -> &'static str {
        match self {
            PatchFormat::Freeform => "codex",
            PatchFormat::Codex => "codex",
            PatchFormat::Unified => "unified",
        }
    }

    fn result_name(self) -> &'static str {
        match self {
            PatchFormat::Freeform => "freeform",
            PatchFormat::Codex => "codex",
            PatchFormat::Unified => "unified",
        }
    }
}

fn patch_format(
    arguments: &Map<String, Value>,
    patch: &str,
) -> Result<PatchFormat, LocalToolError> {
    match string_arg_with_default(arguments, "format", "auto")?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "auto" => {
            if patch.trim_start().starts_with("*** Begin Patch") {
                Ok(PatchFormat::Codex)
            } else {
                Ok(PatchFormat::Unified)
            }
        }
        "freeform" => Ok(PatchFormat::Freeform),
        "codex" => Ok(PatchFormat::Codex),
        "unified" => Ok(PatchFormat::Unified),
        other => Err(LocalToolError::InvalidArguments(format!(
            "unsupported patch format {other}; expected auto, freeform, codex, or unified"
        ))),
    }
}

fn mark_patch_result_format(mut result: Value, format: PatchFormat) -> Value {
    if let Value::Object(object) = &mut result {
        object.insert(
            "format".to_string(),
            Value::String(format.result_name().to_string()),
        );
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchTargetPaths {
    LocalSpecial,
    RemoteDefault,
    Unknown,
}

fn classify_patch_target_paths(
    format: PatchFormat,
    patch: &str,
    workspace_root: &Path,
) -> Result<PatchTargetPaths, LocalToolError> {
    let mut saw_local = false;
    let mut saw_remote = false;
    match format {
        PatchFormat::Freeform | PatchFormat::Codex => {
            for path in codex_patch_paths(patch)? {
                if is_local_special_patch_path(&path, workspace_root) {
                    saw_local = true;
                } else {
                    saw_remote = true;
                }
            }
        }
        PatchFormat::Unified => {
            for path in unified_patch_header_paths(patch) {
                if path == "/dev/null" {
                    continue;
                }
                if is_local_special_patch_path(Path::new(path), workspace_root) {
                    saw_local = true;
                } else {
                    saw_remote = true;
                }
            }
        }
    }
    match (saw_local, saw_remote) {
        (true, false) => Ok(PatchTargetPaths::LocalSpecial),
        (false, true) => Ok(PatchTargetPaths::RemoteDefault),
        (false, false) => Ok(PatchTargetPaths::Unknown),
        (true, true) => Err(LocalToolError::InvalidArguments(
            "apply_patch cannot mix local .stellaclaw/workspace absolute paths and remote workspace paths in one patch while fixed remote mode is active".to_string(),
        )),
    }
}

fn codex_patch_paths(patch: &str) -> Result<Vec<PathBuf>, LocalToolError> {
    let normalized = patch.replace("\r\n", "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let mut index = 0usize;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    expect_codex_patch_line(&lines, index, "*** Begin Patch")?;
    index += 1;

    let mut paths = Vec::new();
    let mut saw_end = false;
    while let Some(line) = lines.get(index).copied() {
        if line == "*** End Patch" {
            index += 1;
            saw_end = true;
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            paths.push(safe_patch_path(path)?);
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            paths.push(safe_patch_path(path)?);
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            paths.push(safe_patch_path(path)?);
        } else if let Some(path) = line.strip_prefix("*** Move to: ") {
            paths.push(safe_patch_path(path)?);
        }
        index += 1;
    }

    if !saw_end {
        return Err(LocalToolError::InvalidArguments(
            "codex patch missing *** End Patch".to_string(),
        ));
    }
    if lines[index..].iter().any(|line| !line.trim().is_empty()) {
        return Err(LocalToolError::InvalidArguments(
            "unexpected content after *** End Patch".to_string(),
        ));
    }
    if paths.is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "codex patch must contain at least one file operation".to_string(),
        ));
    }
    Ok(paths)
}

fn unified_patch_header_paths(patch: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            paths.push(split_unified_header_path(rest).0);
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            paths.push(split_unified_header_path(rest).0);
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            let mut parts = rest.split_whitespace();
            if let Some(path) = parts.next() {
                paths.push(path);
            }
            if let Some(path) = parts.next() {
                paths.push(path);
            }
        }
    }
    paths
}

fn is_local_special_patch_path(path: &Path, workspace_root: &Path) -> bool {
    local_special_relative_path(path, workspace_root).is_some()
}

fn local_special_relative_path(path: &Path, workspace_root: &Path) -> Option<PathBuf> {
    let path = strip_unified_side_prefix(path);
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(workspace_root) {
            return relative_stellaclaw_path(relative);
        }
        return absolute_stellaclaw_tail(path);
    }
    relative_stellaclaw_path(path)
}

fn relative_stellaclaw_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return None;
    };
    if first.to_string_lossy() != ".stellaclaw" {
        return None;
    }
    normalized.push(first);
    for component in components {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(normalized)
}

fn absolute_stellaclaw_tail(path: &Path) -> Option<PathBuf> {
    let components = path.components().collect::<Vec<_>>();
    let start = components.iter().position(|component| match component {
        Component::Normal(part) => part.to_string_lossy() == ".stellaclaw",
        _ => false,
    })?;
    let mut normalized = PathBuf::new();
    for component in &components[start..] {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(normalized)
}

fn strip_unified_side_prefix(path: &Path) -> &Path {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return path;
    };
    let first = first.to_string_lossy();
    if first == "a" || first == "b" {
        components.as_path()
    } else {
        path
    }
}

#[cfg(not(test))]
fn ensure_fs_tool_local(context: &ToolExecutionContext<'_>) -> Result<PathBuf, LocalToolError> {
    let response = ensure_tool_binary(context, FS_TOOL_NAME, None)?;
    let path = response.local_path.ok_or_else(|| {
        LocalToolError::Bridge("tool_binary_ensure did not return local_path".to_string())
    })?;
    Ok(PathBuf::from(path))
}

fn ensure_fs_tool_remote(
    context: &ToolExecutionContext<'_>,
    host: &str,
) -> Result<String, LocalToolError> {
    let response = ensure_tool_binary(context, FS_TOOL_NAME, Some(host))?;
    response.remote_path.ok_or_else(|| {
        LocalToolError::Bridge("tool_binary_ensure did not return remote_path".to_string())
    })
}

fn remote_shell_command(script: &str) -> String {
    format!("{REMOTE_SAFE_PATH_PREFIX} {script}")
}

fn parse_fs_tool_json(output: &std::process::Output, remote: Option<&str>) -> Option<Value> {
    let mut value = serde_json::from_slice::<Value>(&output.stdout).ok()?;
    if let (Some(remote), Value::Object(object)) = (remote, &mut value) {
        object.insert("remote".to_string(), Value::String(remote.to_string()));
    }
    Some(value)
}

fn patch_result(
    output: std::process::Output,
    remote: Option<&str>,
    max_output_chars: usize,
) -> Value {
    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();
    let (stdout, stdout_truncated) = truncate_tool_text(&stdout_text, max_output_chars);
    let (stderr, stderr_truncated) = truncate_tool_text(&stderr_text, max_output_chars);

    let mut result = Map::new();
    result.insert("applied".to_string(), Value::Bool(output.status.success()));
    if let Some(returncode) = output.status.code() {
        result.insert("returncode".to_string(), Value::from(returncode));
    }
    if let Some(remote) = remote {
        result.insert("remote".to_string(), Value::String(remote.to_string()));
    }
    if !stdout.is_empty() {
        result.insert("stdout".to_string(), Value::String(stdout));
    }
    if !stderr.is_empty() {
        result.insert("stderr".to_string(), Value::String(stderr));
    }
    if stdout_truncated {
        result.insert("stdout_truncated".to_string(), Value::Bool(true));
    }
    if stderr_truncated {
        result.insert("stderr_truncated".to_string(), Value::Bool(true));
    }
    Value::Object(result)
}

fn expect_codex_patch_line(
    lines: &[&str],
    index: usize,
    expected: &str,
) -> Result<(), LocalToolError> {
    if lines.get(index).copied() == Some(expected) {
        Ok(())
    } else {
        Err(LocalToolError::InvalidArguments(format!(
            "codex patch must start with {expected}"
        )))
    }
}

fn safe_patch_path(path: &str) -> Result<PathBuf, LocalToolError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "patch path must not be empty".to_string(),
        ));
    }
    let path = PathBuf::from(path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(LocalToolError::InvalidArguments(
            "codex patch paths must be relative workspace paths without ..".to_string(),
        ));
    }
    Ok(path)
}

#[cfg(test)]
#[path = "patch_test.rs"]
mod patch_test;
