//! Claude API integration tests
//! Requires ANTHROPIC_API_KEY environment variable or .env.test file

use agent_frame::{
    create_chat_completion, ApiFormat, ChatMessage, Tool, UpstreamConfig,
};
use serde_json::json;
use std::env;

fn load_claude_config() -> Option<UpstreamConfig> {
    // 加载 .env.test 文件
    let _ = dotenvy::from_filename(".env.test")
        .or_else(|_| dotenvy::from_filename("../.env.test"))
        .or_else(|_| dotenvy::dotenv());

    let api_key = env::var("ANTHROPIC_API_KEY").ok()?;
    let base_url = env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string());

    Some(UpstreamConfig {
        base_url,
        model: "claude-3-5-sonnet-20241022".to_string(),
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
        "Failed to load Claude config. Set ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL in .env.test",
    );

    let messages = vec![ChatMessage::text("user", "Hello! Say 'Claude is working' and nothing else.")];

    let result = create_chat_completion(&config, &messages, &[], None);

    match result {
        Ok(outcome) => {
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
        "Failed to load Claude config. Set ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL in .env.test",
    );

    // 定义一个测试工具
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
            println!("Response: {:?}", outcome.message.content);
            println!("Tool calls: {:?}", outcome.message.tool_calls);
            
            // 验证响应或工具调用
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
        "Failed to load Claude config. Set ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL in .env.test",
    );

    let mut messages = vec![
        ChatMessage::text("user", "My name is Alice."),
    ];

    // 第一轮
    let result1 = create_chat_completion(&config, &messages, &[], None)
        .expect("First turn failed");
    println!("Turn 1 response: {:?}", result1.message.content);
    messages.push(result1.message);

    // 第二轮（测试记忆）
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
fn test_claude_config_loading() {
    // 这个测试不需要 API key，验证配置加载逻辑
    let _ = dotenvy::from_filename(".env.test")
        .or_else(|_| dotenvy::from_filename("../.env.test"));

    // 如果没有配置，应该返回 None
    if env::var("ANTHROPIC_API_KEY").is_err() {
        println!("Skipping: ANTHROPIC_API_KEY not set");
        return;
    }

    let config = load_claude_config().expect("Should load config when env vars are set");
    assert_eq!(config.api_format, ApiFormat::Claude);
    assert!(config.api_key.is_some());
    println!("Config loaded successfully: base_url = {}", config.base_url);
}
