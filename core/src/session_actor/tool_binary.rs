use std::{
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

use crossbeam_channel::select_biased;
use serde::{Deserialize, Serialize};

use super::{
    tool_runtime::{LocalToolError, ToolCancellationToken, ToolExecutionContext},
    ConversationBridge, ConversationBridgeRequest,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolBinaryEnsureRequest {
    pub tool: String,
    #[serde(default)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolBinaryEnsureResponse {
    pub status: String,
    pub tool: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_dir: Option<String>,
}

static TOOL_BINARY_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(super) fn ensure_tool_binary(
    context: &ToolExecutionContext<'_>,
    tool: &str,
    host: Option<&str>,
) -> Result<ToolBinaryEnsureResponse, LocalToolError> {
    let Some(bridge) = context.conversation_bridge else {
        return Err(LocalToolError::Bridge(
            "tool binary manager is not configured".to_string(),
        ));
    };
    let request_id = next_tool_binary_request_id(tool);
    let response = bridge
        .call(tool_binary_ensure_bridge_request(&request_id, tool, host)?)
        .map_err(|error| LocalToolError::Bridge(error.to_string()))?;
    parse_tool_binary_ensure_response(response)
}

pub(super) fn ensure_tool_binary_interruptibly(
    context: &ToolExecutionContext<'_>,
    tool: &str,
    host: Option<&str>,
    cancel_token: &ToolCancellationToken,
) -> Result<ToolBinaryEnsureResponse, LocalToolError> {
    let Some(bridge) = context.conversation_bridge.cloned() else {
        return Err(LocalToolError::Bridge(
            "tool binary manager is not configured".to_string(),
        ));
    };
    let request_id = next_tool_binary_request_id(tool);
    let request = tool_binary_ensure_bridge_request(&request_id, tool, host)?;
    let bridge_for_request = bridge.clone();
    let (response_tx, response_rx) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let response = bridge_for_request
            .call(request)
            .map_err(|error| error.to_string());
        let _ = response_tx.send(response);
    });

    select_biased! {
        recv(cancel_token.cancel_rx()) -> _ => {
            send_tool_binary_cancel_request(bridge, &request_id);
            Err(LocalToolError::Io("tool interrupted".to_string()))
        }
        recv(response_rx) -> response => {
            parse_tool_binary_ensure_response(
                response
                    .map_err(|_| LocalToolError::Bridge("tool binary manager stopped".to_string()))?
                    .map_err(LocalToolError::Bridge)?,
            )
        }
    }
}

fn next_tool_binary_request_id(tool: &str) -> String {
    format!(
        "tool_binary_ensure_{tool}_{}",
        TOOL_BINARY_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn tool_binary_ensure_bridge_request(
    request_id: &str,
    tool: &str,
    host: Option<&str>,
) -> Result<ConversationBridgeRequest, LocalToolError> {
    let request = ToolBinaryEnsureRequest {
        tool: tool.to_string(),
        host: host.map(str::to_string),
    };
    Ok(ConversationBridgeRequest {
        request_id: request_id.to_string(),
        tool_call_id: request_id.to_string(),
        tool_name: "tool_binary_ensure".to_string(),
        action: "tool_binary_ensure".to_string(),
        payload: serde_json::to_value(request).map_err(|error| {
            LocalToolError::InvalidArguments(format!(
                "failed to encode tool binary request: {error}"
            ))
        })?,
    })
}

fn send_tool_binary_cancel_request(
    bridge: std::sync::Arc<dyn ConversationBridge + Send + Sync>,
    request_id: &str,
) {
    let request = ConversationBridgeRequest {
        request_id: format!("{request_id}_cancel"),
        tool_call_id: request_id.to_string(),
        tool_name: "tool_binary_ensure".to_string(),
        action: "tool_binary_ensure_cancel".to_string(),
        payload: serde_json::json!({
            "request_id": request_id,
            "reason": "tool_interrupted",
        }),
    };
    thread::spawn(move || {
        let _ = bridge.call(request);
    });
}

fn parse_tool_binary_ensure_response(
    response: super::ConversationBridgeResponse,
) -> Result<ToolBinaryEnsureResponse, LocalToolError> {
    let text = crate::session_actor::tool_result_text(&response.result);
    if text.trim().is_empty() {
        return Err(LocalToolError::Bridge(
            "tool binary response missing result".to_string(),
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        LocalToolError::Bridge(format!(
            "failed to parse tool binary response: {error}: {text}"
        ))
    })?;
    if value.get("status").and_then(serde_json::Value::as_str) != Some("success") {
        let reason = value
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(text.as_str());
        return Err(LocalToolError::Bridge(reason.to_string()));
    }
    let parsed: ToolBinaryEnsureResponse = serde_json::from_value(value).map_err(|error| {
        LocalToolError::Bridge(format!(
            "failed to parse tool binary response: {error}: {text}"
        ))
    })?;
    if parsed.status == "success" {
        Ok(parsed)
    } else {
        Err(LocalToolError::Bridge(text))
    }
}
