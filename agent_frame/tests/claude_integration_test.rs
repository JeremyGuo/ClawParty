//! Claude API integration tests
//! Requires ANTHROPIC_API_KEY environment variable or .env.test file
//!
//! Supports:
//! - Anthropic Claude API
//! - Kimi Coding (Claude-compatible)
//!
//! Example .env.test:
//! ```
//! ANTHROPIC_BASE_URL=https://api.kimi.com/coding
//! ANTHROPIC_API_KEY=sk-kimi-...
//! TEST_MODEL=kimi-k2.5-coding
//! ```

use agent_frame::{
    create_chat_completion, ApiFormat, ChatMessage, Tool, UpstreamConfig,
};
use serde_json::json;
use std::env;

/// Load Claude/Kimi API configuration from environment
fn load_claude_config() -> Option<UpstreamConfig> {
    // Load .env.test or .env file
    let _ = dotenvy::from_filename(".env.test")
        .or_else(|_| dotenvy::from_filename("../.env.test"))
        .or_else(|_| dotenvy::dotenv());

    let api_key = env::var("ANTHROPIC_API_KEY").ok()?;
    let base_url = env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let model = env::var("TEST_MODEL")
        .unwrap_or_else(|_| "claude-3-5-sonnet-20241022".to_string());

    Some(UpstreamConfig {
        base_url,
        model,
        supports_vision_input: false,
        api_key: Some(api_key),
        api_key_env: "ANTHROPIC_API_KEY".to_string(),
        chat_completions_path: "/v1/messages".to_string(),
        timeout_seconds: 60.0,
        context_window_tokens: 200_000,
        cache_control: None,
        reasoning: None,
        headers: serde_json::Map::new(),
        native_web_search: None,
        external_web_search: None,
        api_format: ApiFormat::Claude,
    })
}

#[test]
#[ignore = "requires real API key"]
fn test_claude_simple_completion() {
    let config = load_claude_config().expect(
        "Failed to load config. Set ANTHROPIC_API_KEY and optionally ANTHROPIC_BASE_URL in .env.test\n\
         For Kimi Coding: ANTHROPIC_BASE_URL=https://api.kimi.com/coding",
    );

    let messages = vec![ChatMessage::text("user", "Hello! Say 'Claude API is working' and nothing else.")];

    let result = create_chat_completion(&config, &messages, &[], None);

    match result {
        Ok(outcome) => {
            println!("Model: {}", config.model);
            println!("Response: {:?}", outcome.message.content);
            println!("Usage: {:?}", outcome.usage);
            assert!(outcome.usage.total_tokens > 0);
        }
        Err(e) => {
            panic!("API call failed: {}", e);
        }
    }
}

#[test]
#[ignore = "requires real API key"]
fn test_claude_with_tool() {
    let config = load_claude_config().expect(
        "Failed to load config. Set ANTHROPIC_API_KEY in .env.test",
    );

    // Define a test tool
    let tools = vec![Tool::new(
        "get_weather",
        "Get the current weather for a location",
        json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "City name"
                }
            },
            "required": ["location"]
        }),
        |args| {
            let location = args["location"].as_str().unwrap_or("unknown");
            Ok(json!({
                "temperature": 22,
                "condition": "sunny",
                "location": location
            }))
        },
    )];

    let messages = vec![ChatMessage::text(
        "user",
        "What's the weather in Beijing? Use the get_weather tool.",
    )];

    let result = create_chat_completion(&config, &messages, &tools, None);

    match result {
        Ok(outcome) => {
            println!("Model: {}", config.model);
            println!("Response: {:?}", outcome.message.content);
            println!("Tool calls: {:?}", outcome.message.tool_calls);
            
            // Verify response or tool calls
            assert!(
                outcome.message.content.is_some() || outcome.message.tool_calls.is_some(),
                "Expected either content or tool calls"
            );
        }
        Err(e) => {
            panic!("API call failed: {}", e);
        }
    }
}

#[test]
#[ignore = "requires real API key"]
fn test_claude_multi_turn_conversation() {
    let config = load_claude_config().expect(
        "Failed to load config. Set ANTHROPIC_API_KEY in .env.test",
    );

    let mut messages = vec![
        ChatMessage::text("user", "My name is Alice."),
    ];

    // Turn 1
    let result1 = create_chat_completion(&config, &messages, &[], None)
        .expect("First turn failed");
    println!("Turn 1 response: {:?}", result1.message.content);
    messages.push(result1.message);

    // Turn 2 (test memory)
    messages.push(ChatMessage::text("user", "What's my name?"));
    let result2 = create_chat_completion(&config, &messages, &[], None)
        .expect("Second turn failed");
    
    let response_text = result2
        .message
        .content
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    
    println!("Turn 2 response: {:?}", result2.message.content);
    assert!(
        response_text.contains("alice"),
        "Expected Claude to remember the name 'Alice', got: {}",
        response_text
    );
}

#[test]
fn test_config_loading() {
    // This test doesn't need API key, verifies config loading logic
    let _ = dotenvy::from_filename(".env.test")
        .or_else(|_| dotenvy::from_filename("../.env.test"));

    // If no config, should return None
    if env::var("ANTHROPIC_API_KEY").is_err() {
        println!("Skipping: ANTHROPIC_API_KEY not set");
        return;
    }

    let config = load_claude_config().expect("Should load config when env vars are set");
    assert_eq!(config.api_format, ApiFormat::Claude);
    assert!(config.api_key.is_some());
    assert!(config.chat_completions_path.contains("/messages"));
    println!("Config loaded successfully:");
    println!("  base_url: {}", config.base_url);
    println!("  model: {}", config.model);
    println!("  api_format: {:?}", config.api_format);
}
