//! Anthropic Claude API format implementation

use super::{
    ChatCompletionOutcome, TokenUsage, build_chat_completions_url,
    upstream_error_from_value,
};
use crate::config::UpstreamConfig;
use crate::message::{ChatMessage, FunctionCall, ToolCall};
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::Duration;

/// Claude API request body
#[derive(Serialize)]
struct ClaudeRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<ClaudeMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ClaudeTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
}

/// Claude message format
#[derive(Serialize, Deserialize, Debug)]
struct ClaudeMessage {
    role: String,
    content: Vec<ClaudeContentBlock>,
}

/// Claude content block (can be text, image, or tool_use)
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
enum ClaudeContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

/// Claude tool definition
#[derive(Serialize, Debug)]
struct ClaudeTool {
    name: String,
    description: String,
    input_schema: Value,
}

/// Claude API response
#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct ClaudeResponse {
    id: String,
    #[serde(rename = "type")]
    response_type: String,
    role: String,
    content: Vec<ClaudeContentBlock>,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<ClaudeUsage>,
}

/// Claude usage statistics
#[derive(Deserialize, Debug)]
struct ClaudeUsage {
    input_tokens: u64,
    output_tokens: u64,
}

pub fn create_chat_completion(
    upstream: &UpstreamConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    extra_payload: Option<Map<String, Value>>,
) -> Result<ChatCompletionOutcome> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs_f64(upstream.timeout_seconds))
        .build()
        .context("failed to construct Claude client")?;

    // Convert OpenAI format messages to Claude format
    let (system_prompt, claude_messages) = convert_messages_to_claude(messages)?;

    // Convert tools to Claude format
    let claude_tools = if tools.is_empty() {
        None
    } else {
        Some(
            tools
                .iter()
                .map(|tool| ClaudeTool {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.parameters.clone(),
                })
                .collect(),
        )
    };

    let mut request = ClaudeRequest {
        model: upstream.model.clone(),
        system: system_prompt,
        messages: claude_messages,
        tools: claude_tools,
        tool_choice: if tools.is_empty() {
            None
        } else {
            Some(json!({ "type": "auto" }))
        },
        max_tokens: Some(4096),
        temperature: Some(0.0),
        thinking: None,
    };

    // Handle extra_payload (merge into request if applicable)
    if let Some(extra) = extra_payload {
        if let Some(max_tokens) = extra.get("max_tokens").and_then(Value::as_u64) {
            request.max_tokens = Some(max_tokens as u32);
        }
        if let Some(temp) = extra.get("temperature").and_then(Value::as_f64) {
            request.temperature = Some(temp as f32);
        }
        if extra.contains_key("thinking") {
            request.thinking = extra.get("thinking").cloned();
        }
    }

    let mut http_request = client
        .post(build_chat_completions_url(upstream))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&request);

    // Claude uses x-api-key header instead of Bearer
    if let Some(api_key) = upstream
        .api_key
        .clone()
        .or_else(|| std::env::var(&upstream.api_key_env).ok())
    {
        http_request = http_request.header("x-api-key", api_key);
    }

    // Add custom headers
    for (key, value) in &upstream.headers {
        if let Some(value) = value.as_str() {
            http_request = http_request.header(key, value);
        }
    }

    let response = http_request
        .send()
        .context("Claude API request failed")?;
    
    let status = response.status();
    let body = response
        .text()
        .context("failed to read Claude response body")?;
    
    if !status.is_success() {
        return Err(anyhow!(
            "Claude API failed with {}: {}",
            status,
            body
        ));
    }

    let value: Value =
        serde_json::from_str(&body).context("failed to parse Claude response")?;
    
    if let Some(error_message) = upstream_error_from_value(&value) {
        return Err(anyhow!(
            "Claude API returned an error: {}",
            error_message
        ));
    }

    let claude_response: ClaudeResponse =
        serde_json::from_value(value).context("failed to decode Claude response")?;

    // Convert Claude response back to ChatMessage format
    let (message, usage) = convert_claude_response(claude_response);

    Ok(ChatCompletionOutcome { message, usage })
}

/// Convert OpenAI-style messages to Claude format
/// Returns (system_prompt, messages)
fn convert_messages_to_claude(
    messages: &[ChatMessage],
) -> Result<(Option<String>, Vec<ClaudeMessage>)> {
    let mut system_parts = Vec::new();
    let mut claude_messages = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                // Extract system message content
                if let Some(content) = &msg.content {
                    let text = content_to_text(content);
                    if !text.is_empty() {
                        system_parts.push(text);
                    }
                }
            }
            "user" => {
                let content = if let Some(content) = &msg.content {
                    vec![ClaudeContentBlock::Text {
                        text: content_to_text(content),
                    }]
                } else {
                    vec![]
                };
                claude_messages.push(ClaudeMessage {
                    role: "user".to_string(),
                    content,
                });
            }
            "assistant" => {
                let mut content_blocks = Vec::new();
                
                // Add text content if present
                if let Some(text) = msg.content.as_ref().map(content_to_text) {
                    if !text.is_empty() {
                        content_blocks.push(ClaudeContentBlock::Text { text });
                    }
                }
                
                // Add tool calls as tool_use blocks
                if let Some(tool_calls) = &msg.tool_calls {
                    for call in tool_calls {
                        let input: Value = serde_json::from_str(
                            call.function.arguments.as_deref().unwrap_or("{}")
                        ).unwrap_or(json!({}));
                        
                        content_blocks.push(ClaudeContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.function.name.clone(),
                            input,
                        });
                    }
                }
                
                claude_messages.push(ClaudeMessage {
                    role: "assistant".to_string(),
                    content: content_blocks,
                });
            }
            "tool" => {
                // Tool results in Claude go as user messages with tool_result blocks
                let tool_call_id = msg.tool_call_id.clone().unwrap_or_default();
                let content = msg
                    .content
                    .as_ref()
                    .map(content_to_text)
                    .unwrap_or_default();
                
                claude_messages.push(ClaudeMessage {
                    role: "user".to_string(),
                    content: vec![ClaudeContentBlock::ToolResult {
                        tool_use_id: tool_call_id,
                        content,
                    }],
                });
            }
            _ => {
                // Unknown role, treat as user message
                let text = msg
                    .content
                    .as_ref()
                    .map(content_to_text)
                    .unwrap_or_default();
                claude_messages.push(ClaudeMessage {
                    role: "user".to_string(),
                    content: vec![ClaudeContentBlock::Text { text }],
                });
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    Ok((system, claude_messages))
}

/// Convert Claude response back to ChatMessage format
fn convert_claude_response(response: ClaudeResponse) -> (ChatMessage, TokenUsage) {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in response.content {
        match block {
            ClaudeContentBlock::Text { text } => {
                text_parts.push(text);
            }
            ClaudeContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: Some(input.to_string()),
                    },
                });
            }
            _ => {}
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(Value::String(text_parts.join("\n")))
    };

    let message = ChatMessage {
        role: "assistant".to_string(),
        content,
        name: None,
        tool_call_id: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
    };

    let usage = response.usage.map_or_else(
        TokenUsage::default,
        |u| TokenUsage {
            llm_calls: 1,
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    );

    (message, usage)
}

fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            // Handle multi-modal content
            let mut parts = Vec::new();
            for item in items {
                if let Some(obj) = item.as_object() {
                    if let Some(text) = obj.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
            }
            if parts.is_empty() {
                content.to_string()
            } else {
                parts.join("\n")
            }
        }
        other => other.to_string(),
    }
}

use serde_json::json;
