mod common;
mod fetch;
mod search;

use std::sync::Arc;

use super::{ToolDefinition, ToolEntry};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebSearchOptions {
    pub enabled: bool,
    pub web: bool,
    pub image: bool,
    pub video: bool,
    pub news: bool,
}

pub fn web_tool_definitions(search_options: WebSearchOptions) -> Vec<ToolDefinition> {
    let mut tools = vec![fetch::WebFetchTool.tool_definition()];

    if search_options.enabled {
        tools.push(search::WebSearchTool::new(search_options).tool_definition());
    }

    tools
}

pub(crate) fn web_tool_entries(search_options: WebSearchOptions) -> Vec<ToolEntry> {
    let mut entries = vec![ToolEntry::Base(Arc::new(fetch::WebFetchTool))];
    if search_options.enabled {
        entries.push(ToolEntry::Base(Arc::new(search::WebSearchTool::new(
            search_options,
        ))));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::WebSearchOptions;
    use crate::model_config::{
        ModelCapability, ModelConfig, ProviderType, RetryMode, TokenEstimatorType,
    };
    use crate::session_actor::tool_catalog::web_tools::{
        fetch::WebFetchTool, search::WebSearchTool,
    };
    use crate::session_actor::SearchToolModels;
    use serde_json::{json, Map, Value};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn web_fetch_defaults_and_strips_html() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/doc")
            .match_header("user-agent", "stellaclaw-core/0.1")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><head><title>Example Doc</title><meta name=\"description\" content=\"A &amp; B\"><style>.x{}</style></head><body><h1>Hello</h1><script>ignore()</script><p>World &amp; docs</p><a href=\"/next\">Next page</a></body></html>")
            .create();
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            Value::String(format!("{}/doc", server.url())),
        );

        let result = WebFetchTool
            .fetch(&arguments)
            .expect("fetch should succeed");

        assert_eq!(result["kind"], "web_fetch_result");
        assert_eq!(result["status"], 200);
        assert_eq!(result["ok"], true);
        assert_eq!(result["body_format"], "text");
        assert_eq!(result["title"], "Example Doc");
        assert_eq!(result["description"], "A & B");
        assert_eq!(result["links"][0]["text"], "Next page");
        assert_eq!(result["links"][0]["url"], format!("{}/next", server.url()));
        assert_eq!(result["body"], "Hello\n\nWorld & docs\n\nNext page");
    }

    #[test]
    fn web_fetch_raw_format_preserves_html() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/raw")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<h1>Raw</h1>")
            .create();
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            Value::String(format!("{}/raw", server.url())),
        );
        arguments.insert("format".to_string(), Value::String("raw".to_string()));

        let result = WebFetchTool
            .fetch(&arguments)
            .expect("fetch should succeed");

        assert_eq!(result["body_format"], "raw");
        assert_eq!(result["body"], "<h1>Raw</h1>");
    }

    #[test]
    fn web_fetch_caps_body_bytes_before_text_processing() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/large")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("abcdef")
            .create();
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            Value::String(format!("{}/large", server.url())),
        );
        arguments.insert("max_bytes".to_string(), json!(3));

        let result = WebFetchTool
            .fetch(&arguments)
            .expect("fetch should succeed");

        assert_eq!(result["body_format"], "raw");
        assert_eq!(result["body"], "abc");
        assert_eq!(result["body_truncated_by_bytes"], true);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["bytes_read"], 3);
    }

    #[test]
    fn web_fetch_binary_auto_returns_metadata_without_lossy_body() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/image.png")
            .with_status(200)
            .with_header("content-type", "image/png")
            .with_header("etag", "\"abc\"")
            .with_body(vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3])
            .create();
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            Value::String(format!("{}/image.png", server.url())),
        );

        let result = WebFetchTool
            .fetch(&arguments)
            .expect("fetch should succeed");

        assert_eq!(result["body_format"], "binary");
        assert_eq!(result["body"], "");
        assert_eq!(result["bytes_read"], 8);
        assert_eq!(result["headers"]["etag"], "\"abc\"");
    }

    #[test]
    fn brave_web_search_uses_subscription_header_and_compacts_results() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/res/v1/web/search")
            .match_header("x-subscription-token", "brave-secret")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("q".to_string(), "rust async actors".to_string()),
                mockito::Matcher::UrlEncoded("count".to_string(), "20".to_string()),
                mockito::Matcher::UrlEncoded("result_filter".to_string(), "web".to_string()),
                mockito::Matcher::UrlEncoded("text_decorations".to_string(), "false".to_string()),
                mockito::Matcher::UrlEncoded("extra_snippets".to_string(), "true".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "web": {
                        "results": [
                            {
                                "title": "Tokio Tutorial",
                                "url": "https://tokio.rs/tutorial",
                                "description": "Learn async Rust with Tokio.",
                                "extra_snippets": ["Covers tasks and channels."]
                            }
                        ]
                    }
                }"#,
            )
            .create();
        let _env = EnvVarGuard::set("BRAVE_SEARCH_API_KEY_TEST", "brave-secret");
        let model = test_brave_model_config(format!("{}/res/v1/web/search", server.url()));
        let mut arguments = Map::new();
        arguments.insert(
            "query".to_string(),
            Value::String("rust async actors".to_string()),
        );
        arguments.insert("timeout_seconds".to_string(), json!(2.0));
        arguments.insert("max_results".to_string(), json!(50));

        let models = SearchToolModels {
            web: Some(model),
            ..SearchToolModels::default()
        };
        let result = WebSearchTool::new(WebSearchOptions::default())
            .search(&arguments, None, Some(&models))
            .expect("web search should run");

        assert_eq!(result["citations"][0], "https://tokio.rs/tutorial");
        assert_eq!(result["results"][0]["title"], "Tokio Tutorial");
        assert!(result["answer"]
            .as_str()
            .unwrap()
            .contains("Snippet: Learn async Rust with Tokio."));
    }

    #[test]
    fn brave_web_search_rejects_images() {
        let model =
            test_brave_model_config("https://api.search.brave.com/res/v1/web/search".to_string());
        let mut arguments = Map::new();
        arguments.insert("query".to_string(), Value::String("diagram".to_string()));
        arguments.insert("timeout_seconds".to_string(), json!(2.0));
        arguments.insert("images".to_string(), json!(["diagram.png"]));

        let models = SearchToolModels {
            web: Some(model),
            ..SearchToolModels::default()
        };
        let error = WebSearchTool::new(WebSearchOptions::default())
            .search(&arguments, None, Some(&models))
            .expect_err("brave search should reject image inputs");

        assert!(error.to_string().contains("does not support image inputs"));
    }

    #[test]
    fn brave_image_search_uses_subscription_header_and_compacts_results() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/res/v1/images/search")
            .match_header("x-subscription-token", "brave-secret")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("q".to_string(), "architecture".to_string()),
                mockito::Matcher::UrlEncoded("count".to_string(), "200".to_string()),
                mockito::Matcher::UrlEncoded("safesearch".to_string(), "strict".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "images",
                    "results": [
                        {
                            "title": "Modern Building",
                            "url": "https://example.com/building",
                            "thumbnail": {
                                "src": "https://imgs.search.brave.com/thumb",
                                "width": 500,
                                "height": 300
                            },
                            "properties": {
                                "url": "https://example.com/building.jpg",
                                "width": 1200,
                                "height": 720
                            }
                        }
                    ],
                    "extra": {}
                }"#,
            )
            .create();
        let _env = EnvVarGuard::set("BRAVE_SEARCH_API_KEY_TEST", "brave-secret");
        let model = test_brave_image_model_config(format!("{}/res/v1/images/search", server.url()));
        let mut arguments = Map::new();
        arguments.insert(
            "query".to_string(),
            Value::String("architecture".to_string()),
        );
        arguments.insert("timeout_seconds".to_string(), json!(2.0));
        arguments.insert("max_results".to_string(), json!(250));
        arguments.insert("image".to_string(), json!(true));

        let models = SearchToolModels {
            image: Some(model),
            ..SearchToolModels::default()
        };
        let result = WebSearchTool::new(WebSearchOptions::default())
            .search(&arguments, None, Some(&models))
            .expect("image search should run");

        assert_eq!(result["citations"][0], "https://example.com/building");
        assert_eq!(
            result["results"][0]["thumbnail_url"],
            "https://imgs.search.brave.com/thumb"
        );
        assert_eq!(
            result["results"][0]["image_url"],
            "https://example.com/building.jpg"
        );
    }

    #[test]
    fn web_search_image_mode_requires_image_provider() {
        let mut arguments = Map::new();
        arguments.insert("query".to_string(), Value::String("diagram".to_string()));
        arguments.insert("timeout_seconds".to_string(), json!(2.0));
        arguments.insert("image".to_string(), json!(true));
        let models = SearchToolModels::default();

        let error = WebSearchTool::new(WebSearchOptions::default())
            .search(&arguments, None, Some(&models))
            .expect_err("image search should reject missing image provider");

        assert!(error
            .to_string()
            .contains("image results are not configured"));
    }

    fn test_brave_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearch,
            model_name: "brave-web-search".to_string(),
            url,
            api_key_env: "BRAVE_SEARCH_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::WebSearch],
            token_max_context: 0,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 30,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }

    fn test_brave_image_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearchImage,
            model_name: "brave-image-search".to_string(),
            url,
            api_key_env: "BRAVE_SEARCH_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::WebSearch],
            token_max_context: 0,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 30,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }
}
