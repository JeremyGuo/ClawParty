use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use serde_json::{json, Value};
use stellaclaw_core::session_actor::{ChatMessage, ChatMessageItem, FileItem};

#[derive(Debug, Default)]
pub(super) struct MessageSummary {
    pub(super) message_count: usize,
    pub(super) last_message_id: Option<String>,
    pub(super) last_message_index: Option<usize>,
    pub(super) last_message_time: Option<String>,
    pub(super) last_final_message_id: Option<String>,
    pub(super) last_final_message_time: Option<String>,
}

#[derive(Debug, Clone)]
struct WebAttachment {
    id: String,
    item_index: usize,
    file: FileItem,
    open_in_workspace_path: Option<String>,
}

pub(super) fn decorate_message(
    message: &ChatMessage,
    index: usize,
    conversation_id: &str,
    foreground_session_id: &str,
    conversation_root: Option<&Path>,
) -> Value {
    let attachments = collect_web_attachments(message, conversation_id, conversation_root);
    let mut value = serde_json::to_value(message).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        map.insert("index".to_string(), json!(index));
        if !message.message_id.is_empty() {
            map.insert("id".to_string(), json!(message.message_id));
        }
        let rendered_attachments = attachments
            .iter()
            .map(|attachment| {
                web_attachment_value(
                    attachment,
                    conversation_id,
                    foreground_session_id,
                    &message.message_id,
                )
            })
            .collect::<Vec<_>>();
        map.insert(
            "attachments".to_string(),
            Value::Array(rendered_attachments),
        );
        map.insert("attachment_count".to_string(), json!(attachments.len()));
        rewrite_message_text_fields(map, &attachments);
    }
    value
}

fn collect_web_attachments(
    message: &ChatMessage,
    conversation_id: &str,
    conversation_root: Option<&Path>,
) -> Vec<WebAttachment> {
    let mut attachments = Vec::new();
    for (item_index, item) in message.data.iter().enumerate() {
        match item {
            ChatMessageItem::File(file) => {
                attachments.push(web_attachment(item_index, file, conversation_id))
            }
            ChatMessageItem::ToolResult(result) => {
                for file in &result.result.files {
                    attachments.push(web_attachment(item_index, file, conversation_id));
                }
            }
            _ => {}
        }
    }
    if let Some(conversation_root) = conversation_root {
        collect_markdown_workspace_attachments(message, conversation_root, &mut attachments);
    }
    attachments
}

fn collect_markdown_workspace_attachments(
    message: &ChatMessage,
    conversation_root: &Path,
    attachments: &mut Vec<WebAttachment>,
) {
    let mut next_index = message.data.len();
    for text in message_context_texts(message) {
        for target in markdown_link_targets(text) {
            if attachment_for_markdown_target(&target, attachments).is_some() {
                continue;
            }
            let Some(file) = file_item_for_workspace_markdown_target(&target, conversation_root)
            else {
                continue;
            };
            if attachments
                .iter()
                .any(|attachment| attachment.file.uri == file.uri)
            {
                continue;
            }
            let id = web_attachment_id(next_index, &file);
            attachments.push(WebAttachment {
                id,
                item_index: next_index,
                file,
                open_in_workspace_path: Some(normalize_markdown_path(&target)),
            });
            next_index += 1;
        }
    }
}

fn message_context_texts(message: &ChatMessage) -> impl Iterator<Item = &str> {
    message.data.iter().filter_map(|item| match item {
        ChatMessageItem::Context(context) => Some(context.text.as_str()),
        _ => None,
    })
}

fn markdown_link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        targets.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    targets
}

fn file_item_for_workspace_markdown_target(
    target: &str,
    conversation_root: &Path,
) -> Option<FileItem> {
    let target = normalize_markdown_path(target);
    if target.is_empty() || target.starts_with("attachment://") || has_external_scheme(&target) {
        return None;
    }
    let relative_path = safe_relative_path(&target)?;
    let path = conversation_root.join(relative_path);
    let metadata = fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let canonical_root = fs::canonicalize(conversation_root).ok()?;
    let canonical_path = fs::canonicalize(&path).ok()?;
    if !canonical_path.starts_with(&canonical_root) {
        return None;
    }
    Some(FileItem {
        uri: format!("file://{}", canonical_path.display()),
        name: canonical_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToString::to_string),
        media_type: media_type_from_path(&canonical_path),
        width: None,
        height: None,
        state: None,
    })
}

fn safe_relative_path(value: &str) -> Option<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn media_type_from_path(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let media_type = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "pdf" => "application/pdf",
        _ => return None,
    };
    Some(media_type.to_string())
}

fn web_attachment(item_index: usize, file: &FileItem, conversation_id: &str) -> WebAttachment {
    let id = web_attachment_id(item_index, file);
    WebAttachment {
        id,
        item_index,
        file: file.clone(),
        open_in_workspace_path: workspace_path_from_file_uri(&file.uri, conversation_id),
    }
}

fn web_attachment_value(
    attachment: &WebAttachment,
    conversation_id: &str,
    foreground_session_id: &str,
    message_id: &str,
) -> Value {
    let name = attachment
        .file
        .name
        .clone()
        .or_else(|| file_name_from_uri(&attachment.file.uri))
        .unwrap_or_else(|| attachment.id.clone());
    let media_type = attachment.file.media_type.clone();
    let mut value = json!({
        "id": attachment.id.clone(),
        "index": attachment.item_index,
        "kind": attachment_kind(media_type.as_deref()),
        "name": name,
        "filename": name,
        "media_type": media_type,
        "width": attachment.file.width,
        "height": attachment.file.height,
        "state": attachment.file.state.clone(),
        "preview_url": message_attachment_url(conversation_id, foreground_session_id, message_id, &attachment.id, "preview"),
        "download_url": message_attachment_url(conversation_id, foreground_session_id, message_id, &attachment.id, "download"),
    });
    if let (Value::Object(map), Some(path)) = (&mut value, &attachment.open_in_workspace_path) {
        map.insert("open_in_workspace_path".to_string(), json!(path));
    }
    value
}

fn message_attachment_url(
    conversation_id: &str,
    foreground_session_id: &str,
    message_id: &str,
    attachment_id: &str,
    action: &str,
) -> String {
    format!(
        "/api/conversations/{}/foreground_sessions/{}/messages/{}/attachments/{}/{}",
        encode_path_segment(conversation_id),
        encode_path_segment(foreground_session_id),
        encode_path_segment(message_id),
        encode_path_segment(attachment_id),
        encode_path_segment(action),
    )
}

fn attachment_kind(media_type: Option<&str>) -> &'static str {
    match media_type.unwrap_or_default() {
        value if value.starts_with("image/") => "image",
        value if value.starts_with("audio/") => "audio",
        value if value.starts_with("video/") => "video",
        "application/pdf" => "pdf",
        _ => "document",
    }
}

pub(super) fn web_attachment_id(item_index: usize, file: &FileItem) -> String {
    format!("att_{item_index:03x}_{:016x}", stable_hash(&file.uri))
}

pub(super) fn web_attachment_file(
    message: &ChatMessage,
    conversation_id: &str,
    conversation_root: Option<&Path>,
    attachment_id: &str,
) -> Option<FileItem> {
    collect_web_attachments(message, conversation_id, conversation_root)
        .into_iter()
        .find(|attachment| attachment.id == attachment_id)
        .map(|attachment| attachment.file)
}

pub(super) fn web_attachment_for_workspace_markdown_target(
    target: &str,
    item_index: usize,
    conversation_id: &str,
    foreground_session_id: &str,
    message_id: &str,
    conversation_root: &Path,
) -> Option<(String, Value)> {
    let file = file_item_for_workspace_markdown_target(target, conversation_root)?;
    let attachment = WebAttachment {
        id: web_attachment_id(item_index, &file),
        item_index,
        file,
        open_in_workspace_path: Some(normalize_markdown_path(target)),
    };
    Some((
        attachment.id.clone(),
        web_attachment_value(
            &attachment,
            conversation_id,
            foreground_session_id,
            message_id,
        ),
    ))
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn rewrite_message_text_fields(
    map: &mut serde_json::Map<String, Value>,
    attachments: &[WebAttachment],
) {
    for key in [
        "text",
        "rendered_text",
        "text_with_attachment_markers",
        "preview",
    ] {
        if let Some(Value::String(text)) = map.get_mut(key) {
            *text = rewrite_markdown_attachment_paths(text, attachments);
        }
    }
    if let Some(Value::Array(data)) = map.get_mut("data") {
        for item in data {
            rewrite_context_payload(item, attachments);
        }
    }
    if let Some(Value::Array(items)) = map.get_mut("items") {
        for item in items {
            rewrite_context_payload(item, attachments);
        }
    }
}

fn rewrite_context_payload(item: &mut Value, attachments: &[WebAttachment]) {
    let Some(payload) = item.get_mut("payload").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(Value::String(text)) = payload.get_mut("text") {
        *text = rewrite_markdown_attachment_paths(text, attachments);
    }
}

fn rewrite_markdown_attachment_paths(text: &str, attachments: &[WebAttachment]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        let split = start + 2;
        out.push_str(&rest[..split]);
        let after = &rest[split..];
        let Some(end) = after.find(')') else {
            out.push_str(after);
            return out;
        };
        let target = &after[..end];
        if let Some(attachment) = attachment_for_markdown_target(target, attachments) {
            out.push_str("attachment://");
            out.push_str(&attachment.id);
        } else {
            out.push_str(target);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn attachment_for_markdown_target<'a>(
    target: &str,
    attachments: &'a [WebAttachment],
) -> Option<&'a WebAttachment> {
    let target = normalize_markdown_path(target);
    if target.is_empty() || target.starts_with("attachment://") || has_external_scheme(&target) {
        return None;
    }
    if target.contains('/') {
        let exact = attachments.iter().find(|attachment| {
            attachment_path_candidates(attachment)
                .iter()
                .any(|candidate| candidate == &target)
        });
        if exact.is_some() {
            return exact;
        }
    }
    let target_name = file_name_from_path(&target)?;
    let mut matches = attachments
        .iter()
        .filter(|attachment| attachment_file_name(attachment).as_deref() == Some(target_name));
    let first = matches.next()?;
    if matches.next().is_none() {
        Some(first)
    } else {
        None
    }
}

fn attachment_path_candidates(attachment: &WebAttachment) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(path) = &attachment.open_in_workspace_path {
        candidates.push(normalize_markdown_path(path));
    }
    if let Some(path) = file_path_from_file_uri(&attachment.file.uri) {
        candidates.push(normalize_markdown_path(&path));
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn attachment_file_name(attachment: &WebAttachment) -> Option<String> {
    attachment
        .file
        .name
        .clone()
        .or_else(|| file_name_from_uri(&attachment.file.uri))
}

fn workspace_path_from_file_uri(uri: &str, conversation_id: &str) -> Option<String> {
    let path = file_path_from_file_uri(uri)?;
    let marker = format!("/conversations/{conversation_id}/");
    let index = path.find(&marker)? + marker.len();
    let relative = normalize_markdown_path(&path[index..]);
    if relative.is_empty() {
        None
    } else {
        Some(relative)
    }
}

fn file_name_from_uri(uri: &str) -> Option<String> {
    file_path_from_file_uri(uri)
        .or_else(|| Some(uri.to_string()))
        .and_then(|path| file_name_from_path(&path).map(ToString::to_string))
}

pub(super) fn file_path_from_file_uri(uri: &str) -> Option<String> {
    let value = uri.trim();
    if value.is_empty() || !value.starts_with("file://") {
        return None;
    }
    let path = value.strip_prefix("file://").unwrap_or(value);
    Some(percent_decode(path).replace('\\', "/"))
}

fn file_name_from_path(path: &str) -> Option<&str> {
    path.rsplit('/').find(|part| !part.is_empty())
}

fn normalize_markdown_path(value: &str) -> String {
    let value = value
        .split('#')
        .next()
        .unwrap_or(value)
        .split('?')
        .next()
        .unwrap_or(value)
        .trim();
    percent_decode(value)
        .replace('\\', "/")
        .trim_start_matches("./")
        .replace("//", "/")
}

fn has_external_scheme(value: &str) -> bool {
    let Some(index) = value.find(':') else {
        return false;
    };
    value[..index]
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let mut iter = value.as_bytes().iter().copied();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let Some(hi) = iter.next().and_then(hex_value) else {
                bytes.push(byte);
                continue;
            };
            let Some(lo) = iter.next().and_then(hex_value) else {
                bytes.push(byte);
                continue;
            };
            bytes.push((hi << 4) | lo);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        time::{SystemTime, UNIX_EPOCH},
    };
    use stellaclaw_core::session_actor::{
        ChatRole, ContextItem, ToolResultContent, ToolResultItem,
    };

    fn image_file(uri: &str, name: &str) -> FileItem {
        FileItem {
            uri: uri.to_string(),
            name: Some(name.to_string()),
            media_type: Some("image/jpeg".to_string()),
            width: Some(320),
            height: Some(200),
            state: None,
        }
    }

    fn context_message(text: &str, files: Vec<FileItem>) -> ChatMessage {
        let mut data = vec![ChatMessageItem::Context(ContextItem {
            text: text.to_string(),
        })];
        data.extend(files.into_iter().map(ChatMessageItem::File));
        ChatMessage::new(ChatRole::Assistant, data).with_message_id("msg_1")
    }

    fn context_text(value: &Value) -> &str {
        value["data"][0]["payload"]["text"].as_str().unwrap()
    }

    fn temp_conversation_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("stellaclaw-web-history-{name}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn decorate_message_projects_file_attachment_and_rewrites_markdown() {
        let file = image_file(
            "file:///workdir/conversations/STS2%20Main/output/pass174_cape_inpaint_candidates_board.jpg",
            "pass174_cape_inpaint_candidates_board.jpg",
        );
        let message = context_message(
            "rendered: ![board](output/pass174_cape_inpaint_candidates_board.jpg)",
            vec![file.clone()],
        );

        let value = decorate_message(&message, 7, "STS2 Main", "main", None);
        let attachment = &value["attachments"][0];
        let id = web_attachment_id(1, &file);

        assert_eq!(value["index"], 7);
        assert_eq!(attachment["id"], id);
        assert_eq!(attachment["kind"], "image");
        assert_eq!(
            attachment["name"],
            "pass174_cape_inpaint_candidates_board.jpg"
        );
        assert_eq!(
            attachment["open_in_workspace_path"],
            "output/pass174_cape_inpaint_candidates_board.jpg"
        );
        assert_eq!(attachment["preview_url"], format!("/api/conversations/STS2%20Main/foreground_sessions/main/messages/msg_1/attachments/{id}/preview"));
        assert!(context_text(&value).contains(&format!("attachment://{id}")));
    }

    #[test]
    fn decorate_message_preserves_client_message_id() {
        let message =
            context_message("hello", Vec::new()).with_client_message_id("local-attachment-send-1");

        let value = decorate_message(&message, 9, "c1", "main", None);

        assert_eq!(value["id"], "msg_1");
        assert_eq!(value["client_message_id"], "local-attachment-send-1");
    }

    #[test]
    fn markdown_rewrite_leaves_ambiguous_basename_unchanged() {
        let first = image_file(
            "file:///workdir/conversations/c1/a/output.png",
            "output.png",
        );
        let second = image_file(
            "file:///workdir/conversations/c1/b/output.png",
            "output.png",
        );
        let message = context_message("![ambiguous](output.png)", vec![first, second]);

        let value = decorate_message(&message, 0, "c1", "main", None);

        assert_eq!(value["attachment_count"], 2);
        assert_eq!(context_text(&value), "![ambiguous](output.png)");
    }

    #[test]
    fn markdown_rewrite_matches_unique_basename_when_no_path_matches() {
        let file = image_file(
            "file:///workdir/conversations/c1/nested/output.png",
            "output.png",
        );
        let message = context_message("![unique](output.png)", vec![file.clone()]);

        let value = decorate_message(&message, 0, "c1", "main", None);

        assert_eq!(
            context_text(&value),
            format!("![unique](attachment://{})", web_attachment_id(1, &file))
        );
    }

    #[test]
    fn decorate_message_projects_tool_result_files() {
        let file = image_file(
            "file:///workdir/conversations/c1/tool/result.png",
            "result.png",
        );
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolResult(ToolResultItem {
                tool_call_id: "call_1".to_string(),
                tool_name: "image_generation".to_string(),
                result: ToolResultContent::from_json(json!({"status": "ok"}))
                    .with_file(file.clone()),
            })],
        )
        .with_message_id("msg_tool");

        let value = decorate_message(&message, 2, "c1", "fg_1", None);
        let id = web_attachment_id(0, &file);

        assert_eq!(value["attachment_count"], 1);
        assert_eq!(value["attachments"][0]["id"], id);
        assert_eq!(value["attachments"][0]["download_url"], format!("/api/conversations/c1/foreground_sessions/fg_1/messages/msg_tool/attachments/{id}/download"));
    }

    #[test]
    fn non_file_uri_does_not_become_workspace_path() {
        let file = image_file(
            "https://example.test/conversations/c1/remote.png",
            "remote.png",
        );
        let message = context_message("![remote](remote.png)", vec![file.clone()]);

        let value = decorate_message(&message, 0, "c1", "main", None);

        assert!(value["attachments"][0]["open_in_workspace_path"].is_null());
        assert_eq!(
            file_path_from_file_uri("https://example.test/file.png"),
            None
        );
        assert_eq!(
            context_text(&value),
            format!("![remote](attachment://{})", web_attachment_id(1, &file))
        );
    }

    #[test]
    fn percent_encoded_file_uri_paths_are_decoded_for_matching() {
        let file = image_file(
            "file:///workdir/conversations/c1/output/final%20board.png",
            "final board.png",
        );
        let message = context_message("![board](output/final%20board.png)", vec![file.clone()]);

        let value = decorate_message(&message, 0, "c1", "main", None);

        assert_eq!(
            context_text(&value),
            format!("![board](attachment://{})", web_attachment_id(1, &file))
        );
    }

    #[test]
    fn markdown_workspace_file_is_projected_as_attachment() {
        let root = temp_conversation_root("markdown-workspace-file");
        let output = root.join("ClawParty/tmp");
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("preview.png"), b"png bytes").unwrap();
        let message = context_message("![preview](ClawParty/tmp/preview.png)", Vec::new());

        let value = decorate_message(&message, 0, "c1", "main", Some(&root));

        assert_eq!(value["attachment_count"], 1);
        assert_eq!(value["attachments"][0]["name"], "preview.png");
        assert_eq!(value["attachments"][0]["media_type"], "image/png");
        assert_eq!(
            value["attachments"][0]["open_in_workspace_path"],
            "ClawParty/tmp/preview.png"
        );
        assert!(context_text(&value).contains("attachment://att_"));
        let id = value["attachments"][0]["id"].as_str().unwrap();
        let file = web_attachment_file(&message, "c1", Some(&root), id).unwrap();
        assert!(file.uri.starts_with("file://"));
        assert!(file.uri.ends_with("/ClawParty/tmp/preview.png"));
        fs::remove_dir_all(root).unwrap();
    }
}
