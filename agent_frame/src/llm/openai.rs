//! OpenAI API format implementation

use super::{
    ChatCompletionOutcome, TokenUsage, build_chat_completions_url,
    first_u64, upstream_error_from_value,
};
use crate::config::UpstreamConfig;
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::time::Duration;

#[derive(Deserialize)]
struct ChatCompletionChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
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
        .context("failed to construct OpenAI client")?;

    let mut payload = Map::new();
    payload.insert("model".to_string(), Value::String(upstream.model.clone()));
    payload.insert(
        "messages".to_string(),
        serde_json::to_value(messages).context("failed to serialize messages")?,
    );
    
    if let Some(cache_control) = &upstream.cache_control {
        payload.insert(
            "cache_control".to_string(),
            serde_json::to_value(cache_control).context("failed to serialize cache_control")?,
        );
    }
    
    if let Some(reasoning) = &upstream.reasoning {
        payload.insert(
            "reasoning".to_string(),
            serde_json::to_value(reasoning).context("failed to serialize reasoning config")?,
        );
    }
    
    if let Some(native_web_search) = &upstream.native_web_search
        && native_web_search.enabled
    {
        for (key, value) in &native_web_search.payload {
            payload.insert(key.clone(), value.clone());
        }
    }
    
    if !tools.is_empty() {
        payload.insert(
            "tools".to_string(),
            Value::Array(tools.iter().map(Tool::as_openai_tool).collect()),
        );
        payload.insert("tool_choice".to_string(), Value::String("auto".to_string()));
    }
    
    if let Some(extra_payload) = extra_payload {
        for (key, value) in extra_payload {
            payload.insert(key, value);
        }
    }

    let mut request = client
        .post(build_chat_completions_url(upstream))
        .json(&Value::Object(payload));

    if let Some(api_key) = upstream
        .api_key
        .clone()
        .or_else(|| std::env::var(&upstream.api_key_env).ok())
    {
        request = request.bearer_auth(api_key);
    }

    for (key, value) in &upstream.headers {
        if let Some(value) = value.as_str() {
            request = request.header(key, value);
        }
    }

    let response = request
        .send()
        .context("upstream chat completion request failed")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read upstream response body")?;
    
    if !status.is_success() {
        return Err(anyhow!(
            "upstream chat completion failed with {}: {}",
            status,
            body
        ));
    }

    let value: Value =
        serde_json::from_str(&body).context("failed to parse chat completion response")?;
    
    if let Some(error_message) = upstream_error_from_value(&value) {
        return Err(anyhow!(
            "upstream chat completion returned an error payload: {}",
            error_message
        ));
    }
    
    let usage = parse_usage(&value);
    let parsed: ChatCompletionResponse =
        serde_json::from_value(value).context("failed to decode chat completion response")?;
    let message = parsed
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message)
        .ok_or_else(|| anyhow!("invalid chat completion response: missing choices[0].message"))?;
    
    Ok(ChatCompletionOutcome { message, usage })
}

fn parse_usage(response: &Value) -> TokenUsage {
    let usage = response.get("usage").and_then(Value::as_object);
    let prompt_tokens = usage
        .and_then(|value| {
            first_u64(
                value,
                &[
                    &["prompt_tokens"],
                    &["input_tokens"],
                    &["input_tokens_details", "total_tokens"],
                ],
            )
        })
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|value| {
            first_u64(
                value,
                &[
                    &["completion_tokens"],
                    &["output_tokens"],
                    &["output_tokens_details", "total_tokens"],
                ],
            )
        })
        .unwrap_or(0);
    let total_tokens = usage
        .and_then(|value| first_u64(value, &[&["total_tokens"]]))
        .unwrap_or_else(|| prompt_tokens + completion_tokens);
    let cache_read_tokens = usage
        .and_then(|value| {
            first_u64(
                value,
                &[
                    &["cache_read_input_tokens"],
                    &["prompt_tokens_details", "cached_tokens"],
                    &["input_tokens_details", "cache_read_input_tokens"],
                ],
            )
        })
        .unwrap_or(0);
    let cache_write_tokens = usage
        .and_then(|value| {
            first_u64(
                value,
                &[
                    &["cache_creation_input_tokens"],
                    &["cache_write_input_tokens"],
                    &["prompt_tokens_details", "cache_write_tokens"],
                    &["input_tokens_details", "cache_creation_input_tokens"],
                ],
            )
        })
        .unwrap_or(0);
    let cache_hit_tokens = usage
        .and_then(|value| first_u64(value, &[&["cache_hit_tokens"]]))
        .unwrap_or(cache_read_tokens);
    let cache_miss_tokens = prompt_tokens.saturating_sub(cache_hit_tokens);

    TokenUsage {
        llm_calls: 1,
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cache_hit_tokens,
        cache_miss_tokens,
        cache_read_tokens,
        cache_write_tokens,
    }
}
