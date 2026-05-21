use super::*;

use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    process::{Command, Stdio},
};

use serde_json::json;

#[test]
fn normalizes_absolute_unified_paths_under_base() {
    let patch = "\
diff --git /home/me/work/src/a.py /home/me/work/src/a.py
--- /home/me/work/src/a.py\t2026-05-08
+++ /home/me/work/src/a.py\t2026-05-08
@@ -1 +1 @@
-old
+new
";

    let normalized =
        normalize_unified_patch_paths(patch, Some(Path::new("/home/me/work")), "remote cwd")
            .expect("patch should normalize");

    assert!(normalized.contains("diff --git src/a.py src/a.py"));
    assert!(normalized.contains("--- src/a.py\t2026-05-08"));
    assert!(normalized.contains("+++ src/a.py\t2026-05-08"));
}

#[test]
fn rejects_absolute_unified_paths_outside_base() {
    let patch = "\
--- /other/work/src/a.py
+++ /other/work/src/a.py
@@ -1 +1 @@
-old
+new
";

    let error =
        normalize_unified_patch_paths(patch, Some(Path::new("/home/me/work")), "remote cwd")
            .expect_err("outside path should be rejected");

    assert!(error
        .to_string()
        .contains("outside the remote cwd /home/me/work"));
}

#[test]
fn remote_shell_command_sets_safe_path() {
    let command = remote_shell_command("mkdir -p \"$dir\"");
    assert!(command.starts_with("PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"));
    assert!(command.contains("export PATH;"));
    assert!(command.ends_with("mkdir -p \"$dir\""));
}

#[test]
fn classifies_absolute_workspace_patch_as_local_special() {
    let patch = "\
--- /home/me/work/.stellaclaw/fs_tool_smoke_test.txt
+++ /home/me/work/.stellaclaw/fs_tool_smoke_test.txt
@@ -1 +1 @@
-old
+new
";

    let classification =
        classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/home/me/work"))
            .expect("classification should succeed");

    assert_eq!(classification, PatchTargetPaths::LocalSpecial);
}

#[test]
fn classifies_remote_absolute_stellaclaw_patch_as_local_special() {
    let patch = "\
--- /home/remote/project/.stellaclaw/fs_tool_smoke_test.txt
+++ /home/remote/project/.stellaclaw/fs_tool_smoke_test.txt
@@ -1 +1 @@
-old
+new
";

    let classification =
        classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/local/workspace"))
            .expect("classification should succeed");

    assert_eq!(classification, PatchTargetPaths::LocalSpecial);
}

#[test]
fn normalizes_remote_absolute_stellaclaw_patch_to_local_overlay_path() {
    let patch = "\
diff --git /home/remote/project/.stellaclaw/a.txt /home/remote/project/.stellaclaw/a.txt
--- /home/remote/project/.stellaclaw/a.txt\t2026-05-11
+++ /home/remote/project/.stellaclaw/a.txt\t2026-05-11
@@ -1 +1 @@
-old
+new
";

    let normalized = normalize_local_unified_patch_paths(patch, Path::new("/local/workspace"))
        .expect("patch should normalize");

    assert!(normalized.contains("diff --git .stellaclaw/a.txt .stellaclaw/a.txt"));
    assert!(normalized.contains("--- .stellaclaw/a.txt\t2026-05-11"));
    assert!(normalized.contains("+++ .stellaclaw/a.txt\t2026-05-11"));
}

#[test]
fn classifies_stellaclaw_git_style_patch_as_local_special() {
    let patch = "\
diff --git a/.stellaclaw/a.txt b/.stellaclaw/a.txt
--- a/.stellaclaw/a.txt
+++ b/.stellaclaw/a.txt
@@ -1 +1 @@
-old
+new
";

    let classification =
        classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/home/me/work"))
            .expect("classification should succeed");

    assert_eq!(classification, PatchTargetPaths::LocalSpecial);
}

#[test]
fn classifies_ordinary_relative_patch_as_remote_default() {
    let patch = "\
--- src/main.rs
+++ src/main.rs
@@ -1 +1 @@
-old
+new
";

    let classification =
        classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/home/me/work"))
            .expect("classification should succeed");

    assert_eq!(classification, PatchTargetPaths::RemoteDefault);
}

#[test]
fn rejects_mixed_local_special_and_remote_patch_paths() {
    let patch = "\
--- .stellaclaw/local.txt
+++ .stellaclaw/local.txt
@@ -1 +1 @@
-old
+new
--- src/remote.rs
+++ src/remote.rs
@@ -1 +1 @@
-old
+new
";

    let error =
        classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/home/me/work"))
            .expect_err("mixed paths should be rejected");

    assert!(error.to_string().contains("cannot mix local .stellaclaw"));
}

pub(super) fn apply_local_for_test(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    format: PatchFormat,
    check: bool,
    max_output_chars: usize,
) -> Result<Value, LocalToolError> {
    match format {
        PatchFormat::Freeform | PatchFormat::Codex => {
            apply_codex_patch_local(arguments, workspace_root, format)
        }
        PatchFormat::Unified => {
            apply_unified_patch_local(arguments, workspace_root, check, max_output_chars)
        }
    }
}

fn apply_unified_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    check: bool,
    max_output_chars: usize,
) -> Result<Value, LocalToolError> {
    let patch = normalize_unified_patch_paths(
        &string_arg(arguments, "patch")?,
        Some(workspace_root),
        "local workspace",
    )?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;

    let mut command = Command::new("git");
    command
        .arg("apply")
        .arg("--recount")
        .arg("--whitespace=nowarn")
        .arg(format!("-p{strip}"))
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if reverse {
        command.arg("--reverse");
    }
    if check {
        command.arg("--check");
    }

    let mut child = command
        .spawn()
        .map_err(|error| LocalToolError::Io(format!("failed to spawn git apply: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| LocalToolError::Io("failed to open git apply stdin".to_string()))?
        .write_all(patch.as_bytes())
        .map_err(|error| LocalToolError::Io(format!("failed to write patch: {error}")))?;
    let _ = child.stdin.take();
    let output = child
        .wait_with_output()
        .map_err(|error| LocalToolError::Io(format!("failed to wait for git apply: {error}")))?;
    Ok(patch_result(output, None, max_output_chars))
}

fn normalize_unified_patch_paths(
    patch: &str,
    base: Option<&Path>,
    base_label: &str,
) -> Result<String, LocalToolError> {
    let mut changed = false;
    let mut output = String::with_capacity(patch.len());
    for segment in patch.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        let normalized = if let Some(rest) = line.strip_prefix("--- ") {
            normalize_unified_file_header("--- ", rest, base, base_label, &mut changed)?
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            normalize_unified_file_header("+++ ", rest, base, base_label, &mut changed)?
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            normalize_diff_git_header(rest, base, base_label, &mut changed)?
        } else {
            line.to_string()
        };
        output.push_str(&normalized);
        output.push_str(newline);
    }
    if changed {
        Ok(output)
    } else {
        Ok(patch.to_string())
    }
}

fn normalize_unified_file_header(
    prefix: &str,
    rest: &str,
    base: Option<&Path>,
    base_label: &str,
    changed: &mut bool,
) -> Result<String, LocalToolError> {
    let (path, suffix) = split_unified_header_path(rest);
    let normalized = normalize_unified_path_token(path, base, base_label)?;
    if normalized != path {
        *changed = true;
    }
    Ok(format!("{prefix}{normalized}{suffix}"))
}

fn normalize_diff_git_header(
    rest: &str,
    base: Option<&Path>,
    base_label: &str,
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
    let normalized_old = normalize_unified_path_token(old_path, base, base_label)?;
    let normalized_new = normalize_unified_path_token(new_path, base, base_label)?;
    if normalized_old != old_path || normalized_new != new_path {
        *changed = true;
    }
    Ok(format!("diff --git {normalized_old} {normalized_new}"))
}

fn normalize_unified_path_token(
    path: &str,
    base: Option<&Path>,
    base_label: &str,
) -> Result<String, LocalToolError> {
    if path == "/dev/null" || !Path::new(path).is_absolute() {
        return Ok(path.to_string());
    }
    let Some(base) = base else {
        return Err(LocalToolError::InvalidArguments(format!(
            "unified patch path {path:?} is absolute; use a workspace-relative path"
        )));
    };
    let relative = Path::new(path).strip_prefix(base).map_err(|_| {
        LocalToolError::InvalidArguments(format!(
            "unified patch path {path:?} is absolute and outside the {base_label} {}; use a relative patch path",
            base.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Err(LocalToolError::InvalidArguments(format!(
            "unified patch path {path:?} points at the {base_label} root; use a file path"
        )));
    }
    Ok(relative.display().to_string())
}

fn is_codex_patch_header(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

fn split_patch_line(line: &str) -> Option<(char, &str)> {
    let mut chars = line.chars();
    let kind = chars.next()?;
    if !matches!(kind, ' ' | '-' | '+') {
        return None;
    }
    Some((kind, chars.as_str()))
}

#[derive(Debug, Clone)]
enum TestCodexPatchOp {
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        chunks: Vec<TestCodexPatchChunk>,
    },
}

#[derive(Debug, Clone)]
struct TestCodexPatchChunk {
    old: String,
    new: String,
}

fn apply_codex_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    format: PatchFormat,
) -> Result<Value, LocalToolError> {
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    if strip != 0 || reverse {
        return Err(LocalToolError::InvalidArguments(format!(
            "format={} does not support strip or reverse",
            format.result_name()
        )));
    }
    let patch = string_arg(arguments, "patch")?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let ops = parse_codex_patch_for_test(&patch)?;
    let mut files_changed = BTreeSet::new();
    for op in &ops {
        verify_codex_patch_op(op, workspace_root)?;
    }
    if !check {
        for op in &ops {
            apply_codex_patch_op(op, workspace_root, &mut files_changed)?;
        }
    } else {
        for op in &ops {
            collect_codex_patch_paths(op, &mut files_changed);
        }
    }

    Ok(json!({
        "format": format.result_name(),
        "applied": true,
        "check": check,
        "files_changed": files_changed.iter().cloned().collect::<Vec<_>>(),
        "operation_count": ops.len(),
    }))
}

fn parse_codex_patch_for_test(patch: &str) -> Result<Vec<TestCodexPatchOp>, LocalToolError> {
    let normalized = patch.replace("\r\n", "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let mut index = 0usize;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    expect_codex_patch_line(&lines, index, "*** Begin Patch")?;
    index += 1;
    let mut ops = Vec::new();
    loop {
        let Some(line) = lines.get(index).copied() else {
            return Err(LocalToolError::InvalidArguments(
                "codex patch missing *** End Patch".to_string(),
            ));
        };
        if line == "*** End Patch" {
            index += 1;
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = safe_patch_path(path)?;
            index += 1;
            let mut content = String::new();
            while let Some(line) = lines.get(index).copied() {
                if is_codex_patch_header(line) {
                    break;
                }
                let Some(added_line) = line.strip_prefix('+') else {
                    return Err(LocalToolError::InvalidArguments(
                        "add file lines must start with +".to_string(),
                    ));
                };
                content.push_str(added_line);
                content.push('\n');
                index += 1;
            }
            if content.is_empty() {
                return Err(LocalToolError::InvalidArguments(
                    "add file section must contain at least one + line".to_string(),
                ));
            }
            ops.push(TestCodexPatchOp::Add { path, content });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(TestCodexPatchOp::Delete {
                path: safe_patch_path(path)?,
            });
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = safe_patch_path(path)?;
            index += 1;
            let move_to = if let Some(line) = lines.get(index).copied() {
                if let Some(path) = line.strip_prefix("*** Move to: ") {
                    index += 1;
                    Some(safe_patch_path(path)?)
                } else {
                    None
                }
            } else {
                None
            };
            let mut saw_update_line = false;
            let mut chunks = Vec::new();
            let mut current = TestCodexPatchChunk {
                old: String::new(),
                new: String::new(),
            };
            while let Some(line) = lines.get(index).copied() {
                if is_codex_patch_header(line) {
                    break;
                }
                if line == "*** End of File" {
                    index += 1;
                    continue;
                }
                if line == "@@" || line.starts_with("@@ ") {
                    push_non_empty_chunk(&mut chunks, &mut current);
                    index += 1;
                    continue;
                }
                let Some((kind, text)) = split_patch_line(line) else {
                    return Err(LocalToolError::InvalidArguments(format!(
                        "invalid update line: {line}"
                    )));
                };
                saw_update_line = true;
                match kind {
                    ' ' => {
                        current.old.push_str(text);
                        current.old.push('\n');
                        current.new.push_str(text);
                        current.new.push('\n');
                    }
                    '-' => {
                        current.old.push_str(text);
                        current.old.push('\n');
                    }
                    '+' => {
                        current.new.push_str(text);
                        current.new.push('\n');
                    }
                    _ => unreachable!(),
                }
                index += 1;
            }
            push_non_empty_chunk(&mut chunks, &mut current);
            if move_to.is_none() && !saw_update_line {
                return Err(LocalToolError::InvalidArguments(
                    "update file section must contain changes or a move".to_string(),
                ));
            }
            ops.push(TestCodexPatchOp::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        return Err(LocalToolError::InvalidArguments(format!(
            "unknown codex patch header: {line}"
        )));
    }
    if lines[index..].iter().any(|line| !line.trim().is_empty()) {
        return Err(LocalToolError::InvalidArguments(
            "unexpected content after *** End Patch".to_string(),
        ));
    }
    if ops.is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "codex patch must contain at least one file operation".to_string(),
        ));
    }
    Ok(ops)
}

fn push_non_empty_chunk(chunks: &mut Vec<TestCodexPatchChunk>, current: &mut TestCodexPatchChunk) {
    if current.old.is_empty() && current.new.is_empty() {
        return;
    }
    chunks.push(current.clone());
    current.old.clear();
    current.new.clear();
}

fn verify_codex_patch_op(
    op: &TestCodexPatchOp,
    workspace_root: &Path,
) -> Result<(), LocalToolError> {
    match op {
        TestCodexPatchOp::Add { path, .. } => {
            let target = workspace_root.join(path);
            if target.exists() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} already exists",
                    path.display()
                )));
            }
        }
        TestCodexPatchOp::Delete { path } => {
            let target = workspace_root.join(path);
            if !target.is_file() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} is not an existing file",
                    path.display()
                )));
            }
        }
        TestCodexPatchOp::Update {
            path,
            move_to,
            chunks,
        } => {
            let source = workspace_root.join(path);
            if !source.is_file() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} is not an existing file",
                    path.display()
                )));
            }
            if let Some(move_to) = move_to {
                let target = workspace_root.join(move_to);
                if target.exists() && move_to != path {
                    return Err(LocalToolError::InvalidArguments(format!(
                        "{} already exists",
                        move_to.display()
                    )));
                }
            }
            let content = fs::read_to_string(&source).map_err(|error| {
                LocalToolError::Io(format!("failed to read {}: {error}", source.display()))
            })?;
            verify_chunks_match(path, &content, chunks)?;
        }
    }
    Ok(())
}

fn verify_chunks_match(
    path: &Path,
    content: &str,
    chunks: &[TestCodexPatchChunk],
) -> Result<(), LocalToolError> {
    let mut current = content.to_string();
    for chunk in chunks {
        if chunk.old.is_empty() {
            return Err(LocalToolError::InvalidArguments(format!(
                "update chunk for {} has no old/context lines",
                path.display()
            )));
        }
        let matches = current.matches(&chunk.old).count();
        if matches != 1 {
            return Err(LocalToolError::InvalidArguments(format!(
                "update chunk for {} matched {} locations; include more context",
                path.display(),
                matches
            )));
        }
        current = current.replacen(&chunk.old, &chunk.new, 1);
    }
    Ok(())
}

fn apply_codex_patch_op(
    op: &TestCodexPatchOp,
    workspace_root: &Path,
    files_changed: &mut BTreeSet<String>,
) -> Result<(), LocalToolError> {
    match op {
        TestCodexPatchOp::Add { path, content } => {
            let target = workspace_root.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
                })?;
            }
            fs::write(&target, content.as_bytes()).map_err(|error| {
                LocalToolError::Io(format!("failed to write {}: {error}", target.display()))
            })?;
            files_changed.insert(path.display().to_string());
        }
        TestCodexPatchOp::Delete { path } => {
            let target = workspace_root.join(path);
            fs::remove_file(&target).map_err(|error| {
                LocalToolError::Io(format!("failed to delete {}: {error}", target.display()))
            })?;
            files_changed.insert(path.display().to_string());
        }
        TestCodexPatchOp::Update {
            path,
            move_to,
            chunks,
        } => {
            let source = workspace_root.join(path);
            let mut content = fs::read_to_string(&source).map_err(|error| {
                LocalToolError::Io(format!("failed to read {}: {error}", source.display()))
            })?;
            for chunk in chunks {
                content = content.replacen(&chunk.old, &chunk.new, 1);
            }
            let output_path = move_to.as_ref().unwrap_or(path);
            let target = workspace_root.join(output_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
                })?;
            }
            fs::write(&target, content.as_bytes()).map_err(|error| {
                LocalToolError::Io(format!("failed to write {}: {error}", target.display()))
            })?;
            if move_to.as_ref().is_some_and(|move_to| move_to != path) {
                fs::remove_file(&source).map_err(|error| {
                    LocalToolError::Io(format!("failed to delete {}: {error}", source.display()))
                })?;
            }
            files_changed.insert(path.display().to_string());
            if let Some(move_to) = move_to {
                files_changed.insert(move_to.display().to_string());
            }
        }
    }
    Ok(())
}

fn collect_codex_patch_paths(op: &TestCodexPatchOp, files_changed: &mut BTreeSet<String>) {
    match op {
        TestCodexPatchOp::Add { path, .. }
        | TestCodexPatchOp::Delete { path }
        | TestCodexPatchOp::Update { path, .. } => {
            files_changed.insert(path.display().to_string());
        }
    }
    if let TestCodexPatchOp::Update {
        move_to: Some(move_to),
        ..
    } = op
    {
        files_changed.insert(move_to.display().to_string());
    }
}
