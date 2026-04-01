//! LLM Provider implementations
//! Supports OpenAI and Claude API formats

use crate::config::UpstreamConfig;
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub mod claude;
pub mod openai;

/// API format types for different LLM providers
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    /// OpenAI compatible API format (default)
    #[default]
    Openai,
    /// Anthropic Claude API format
    Claude,
}

/// Token usage statistics
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub llm_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl TokenUsage {
    pub fn add_assign(&mut self, other: &TokenUsage) {
        self.llm_calls += other.llm_calls;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.cache_hit_tokens += other.cache_hit_tokens;
        self.cache_miss_tokens += other.cache_miss_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }
}

/// Result of a chat completion request
#[derive(Clone, Debug)]
pub struct ChatCompletionOutcome {
    pub message: ChatMessage,
    pub usage: TokenUsage,
}

/// Create a chat completion request, automatically routing to the correct provider
pub fn create_chat_completion(
    upstream: &UpstreamConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    extra_payload: Option<Map<String, Value>>,
) -> Result<ChatCompletionOutcome> {
    match upstream.api_format {
        ApiFormat::Claude => {
            claude::create_chat_completion(upstream, messages, tools, extra_payload)
        }
        ApiFormat::Openai => {
            openai::create_chat_completion(upstream, messages, tools, extra_payload)
        }
    }
}

/// Extract error message from upstream response
pub(crate) fn upstream_error_from_value(value: &Value) -> Option<String> {
    // Claude format: { "error": { "message": "...", "type": "..." } }
    if let Some(error) = value.get("error") {
        if let Some(obj) = error.as_object() {
            let message = obj.get("message").and_then(Value::as_str);
            let error_type = obj.get("type").and_then(Value::as_str);
            
            match (message, error_type) {
                (Some(msg), Some(ty)) => return Some(format!("{} (type: {})", msg, ty)),
                (Some(msg), None) => return Some(msg.to_string()),
                (None, Some(ty)) => return Some(format!("Claude error type: {}", ty)),
                _ => {}
            }
        }
        // OpenAI format: { "error": { "message": "...", "code": "..." } }
        if let Some(msg) = error.get("message").and_then(Value::as_str) {
            let code = error.get("code").map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                other => other.to_string(),
            });
            return match code {
                Some(c) => Some(format!("{} (code: {})", msg, c)),
                None => Some(msg.to_string()),
            };
        }
        // Simple string error
        if let Some(text) = error.as_str() {
            return Some(text.to_string());
        }
    }
    
    None
}

/// Get the first u64 value from a set of possible paths
pub(crate) fn first_u64(object: &Map<String, Value>, paths: &[&[&str]]) -> Option<u64> {
    paths.iter().find_map(|path| nested_u64(object, path))
}

fn nested_u64(object: &Map<String, Value>, path: &[&str]) -> Option<u64> {
    let mut current = object.get(*path.first()?)?;
    for segment in &path[1..] {
        current = current.as_object()?.get(*segment)?;
    }
    current.as_u64()
}

/// Build the full URL for chat completions
pub(crate) fn build_chat_completions_url(config: &UpstreamConfig) -> String {
    let base = config.base_url.trim_end_matches('/');
    let path = if config.chat_completions_path.starts_with('/') {
        config.chat_completions_path.clone()
    } else {
        format!("/ {}", config.chat_completions_path)
    };
    format!("{}{}", base, path)
}
