use std::{thread, time::Duration};

use crossbeam_channel::select;
use regex::Regex;
use serde_json::{json, Map, Value};
use url::Url;

use super::{
    common::{html_unescape, strip_html},
    WebSearchOptions,
};
use crate::{
    model_config::{ModelCapability, ModelConfig, ProviderType},
    providers::{global_provider_fork_server, ProviderError, ProviderRequestOwned},
    session_actor::{
        tool_catalog::{
            schema::{add_images_property, object_schema, properties},
            BaseTool, ToolBackend, ToolCallContext, ToolDefinition, ToolExecutionMode,
        },
        tool_runtime::{
            bool_arg_with_default, f64_arg_with_default, string_arg, usize_arg_with_default,
            LocalToolError, ToolExecutionContext,
        },
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, SearchToolModels, ToolResultContent,
    },
};

#[cfg(test)]
use crate::providers::{
    BraveSearchImageProvider, BraveSearchNewsProvider, BraveSearchProvider,
    BraveSearchVideoProvider,
};

pub(super) struct WebSearchTool {
    options: WebSearchOptions,
}

impl WebSearchTool {
    pub(super) fn new(options: WebSearchOptions) -> Self {
        Self { options }
    }

    pub(super) fn tool_definition(&self) -> ToolDefinition {
        let mut schema_properties = properties([
            ("query", json!({"type": "string"})),
            ("timeout_seconds", json!({"type": "number"})),
            ("max_results", json!({"type": "integer"})),
            ("image", json!({"type": "boolean"})),
            ("video", json!({"type": "boolean"})),
            ("news", json!({"type": "boolean"})),
        ]);
        add_images_property(&mut schema_properties, false);
        ToolDefinition::new(
            "web_search",
            &web_search_description(self.options),
            object_schema(schema_properties, &["query", "timeout_seconds"]),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        )
    }

    pub(super) fn search(
        &self,
        arguments: &Map<String, Value>,
        context: Option<&ToolExecutionContext<'_>>,
        search_tool_models: Option<&SearchToolModels>,
    ) -> Result<Value, LocalToolError> {
        let query = string_arg(arguments, "query")?;
        let timeout_seconds = f64_arg_with_default(arguments, "timeout_seconds", 30.0)?;
        if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
            return Err(LocalToolError::InvalidArguments(
                "timeout_seconds must be a positive finite number".to_string(),
            ));
        }
        let max_results = usize_arg_with_default(arguments, "max_results", 5)?;
        let vertical = requested_search_vertical(arguments)?;
        if vertical != SearchVertical::Web {
            let Some(search_tool_models) = search_tool_models else {
                return Err(LocalToolError::InvalidArguments(format!(
                    "web_search {} results require a configured provider",
                    vertical.name()
                )));
            };
            return search_with_vertical_provider(
                search_tool_models,
                vertical,
                &query,
                context,
                timeout_seconds,
                max_results,
            );
        }
        if let Some(search_tool_model) = search_tool_models.and_then(|models| models.web.as_ref()) {
            return search_with_provider(
                search_tool_model,
                arguments,
                &query,
                context,
                timeout_seconds,
                max_results.clamp(1, 20),
            );
        }
        let max_results = max_results.clamp(1, 10);

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs_f64(timeout_seconds))
            .user_agent("stellaclaw-core/0.1")
            .build()
            .map_err(|error| LocalToolError::Io(format!("failed to build web client: {error}")))?;

        if let Ok(base_url) = std::env::var("STELLACLAW_WEB_SEARCH_URL") {
            return web_search_json_endpoint(&client, &base_url, &query, max_results);
        }

        let body = client
            .get("https://duckduckgo.com/html/")
            .query(&[("q", query.as_str())])
            .send()
            .map_err(|error| LocalToolError::Io(format!("web_search request failed: {error}")))?
            .text()
            .map_err(|error| {
                LocalToolError::Io(format!("failed to read web_search body: {error}"))
            })?;
        Ok(json!({
            "query": query,
            "results": parse_duckduckgo_html_results(&body, max_results),
        }))
    }
}

impl BaseTool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        self.tool_definition()
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let arguments = object_arguments(args)?;
        self.search(
            &arguments,
            Some(&ctx.execution),
            ctx.execution.search_tool_models,
        )
        .map(ToolResultContent::from_tool_value)
    }
}

fn web_search_description(options: WebSearchOptions) -> String {
    let mut supported = vec!["web"];
    if options.image {
        supported.push("image");
    }
    if options.video {
        supported.push("video");
    }
    if options.news {
        supported.push("news");
    }
    let unsupported = [
        (!options.image).then_some("image=true"),
        (!options.video).then_some("video=true"),
        (!options.news).then_some("news=true"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    let unsupported = if unsupported.is_empty() {
        String::new()
    } else {
        format!(" This session does not support: {unsupported}.")
    };
    format!(
        "Search using the configured provider and return structured results plus citations. Supported result types: {}. Set at most one of image=true, video=true, or news=true; omit them for normal web results.{} If interrupted by a newer user message or timeout observation, this tool cancels the in-flight search result and returns immediately.",
        supported.join(", "),
        unsupported
    )
}

fn object_arguments(args: Value) -> Result<Map<String, Value>, LocalToolError> {
    let Value::Object(arguments) = args else {
        return Err(LocalToolError::InvalidArguments(
            "tool arguments must be a JSON object".to_string(),
        ));
    };
    Ok(arguments)
}

fn search_with_provider(
    model_config: &ModelConfig,
    arguments: &Map<String, Value>,
    query: &str,
    context: Option<&ToolExecutionContext<'_>>,
    timeout_seconds: f64,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    if !model_config.supports(ModelCapability::WebSearch) {
        return Err(LocalToolError::InvalidArguments(
            "the configured search provider does not have web_search capability".to_string(),
        ));
    }
    if arguments
        .get("images")
        .and_then(Value::as_array)
        .is_some_and(|images| !images.is_empty())
    {
        return Err(LocalToolError::InvalidArguments(
            "the configured web search provider does not support image inputs".to_string(),
        ));
    }

    if model_config.provider_type != ProviderType::BraveSearch {
        return Err(LocalToolError::InvalidArguments(format!(
            "unsupported web_search provider {:?}",
            model_config.provider_type
        )));
    }
    let mut model_config = model_config.clone();
    model_config.request_timeout = timeout_seconds.ceil().max(1.0) as u64;
    search_with_provider_worker(&model_config, query, max_results, context)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchVertical {
    Web,
    Image,
    Video,
    News,
}

impl SearchVertical {
    fn name(self) -> &'static str {
        match self {
            SearchVertical::Web => "web",
            SearchVertical::Image => "image",
            SearchVertical::Video => "video",
            SearchVertical::News => "news",
        }
    }
}

fn requested_search_vertical(
    arguments: &Map<String, Value>,
) -> Result<SearchVertical, LocalToolError> {
    let image = bool_arg_with_default(arguments, "image", false)?;
    let video = bool_arg_with_default(arguments, "video", false)?;
    let news = bool_arg_with_default(arguments, "news", false)?;
    let requested = [image, video, news]
        .into_iter()
        .filter(|value| *value)
        .count();
    if requested > 1 {
        return Err(LocalToolError::InvalidArguments(
            "set at most one of image, video, or news to true".to_string(),
        ));
    }
    Ok(if image {
        SearchVertical::Image
    } else if video {
        SearchVertical::Video
    } else if news {
        SearchVertical::News
    } else {
        SearchVertical::Web
    })
}

fn search_with_vertical_provider(
    models: &SearchToolModels,
    vertical: SearchVertical,
    query: &str,
    context: Option<&ToolExecutionContext<'_>>,
    timeout_seconds: f64,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    let model_config = match vertical {
        SearchVertical::Web => models.web.as_ref(),
        SearchVertical::Image => models.image.as_ref(),
        SearchVertical::Video => models.video.as_ref(),
        SearchVertical::News => models.news.as_ref(),
    }
    .ok_or_else(|| {
        LocalToolError::InvalidArguments(format!(
            "web_search {} results are not configured in this session",
            vertical.name()
        ))
    })?;
    if !model_config.supports(ModelCapability::WebSearch) {
        return Err(LocalToolError::InvalidArguments(format!(
            "the configured {} search provider does not have web_search capability",
            vertical.name()
        )));
    }
    let max_results = match (vertical, &model_config.provider_type) {
        (SearchVertical::Image, ProviderType::BraveSearchImage) => max_results.clamp(1, 200),
        (SearchVertical::Video, ProviderType::BraveSearchVideo) => max_results.clamp(1, 50),
        (SearchVertical::News, ProviderType::BraveSearchNews) => max_results.clamp(1, 50),
        _ => {
            return Err(LocalToolError::InvalidArguments(format!(
                "unsupported web_search {} provider {:?}",
                vertical.name(),
                model_config.provider_type,
            )))
        }
    };
    let mut model_config = model_config.clone();
    model_config.request_timeout = timeout_seconds.ceil().max(1.0) as u64;
    search_with_provider_worker(&model_config, query, max_results, context)
}

fn search_with_provider_worker(
    model_config: &ModelConfig,
    query: &str,
    max_results: usize,
    context: Option<&ToolExecutionContext<'_>>,
) -> Result<Value, LocalToolError> {
    #[cfg(test)]
    if context.is_none() {
        return search_with_provider_direct(model_config, query, max_results);
    }

    let fork_server = match global_provider_fork_server() {
        Ok(fork_server) => fork_server,
        Err(error) => {
            #[cfg(test)]
            {
                let _ = error;
                return search_with_provider_direct(model_config, query, max_results);
            }
            #[cfg(not(test))]
            {
                return Err(provider_error_to_local_tool_error(error));
            }
        }
    };

    let messages = vec![ChatMessage::new(
        ChatRole::User,
        vec![ChatMessageItem::Context(ContextItem {
            text: json!({
                "query": query,
                "max_results": max_results,
            })
            .to_string(),
        })],
    )];
    let handle = fork_server
        .start(model_config.clone(), ProviderRequestOwned::new(messages))
        .map_err(provider_error_to_local_tool_error)?;
    let abort_handle = handle.abort_handle();
    let cancel_rx = context
        .map(|context| context.cancel_token.cancel_rx())
        .unwrap_or_else(crossbeam_channel::never);
    let (result_tx, result_rx) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let _ = result_tx.send(handle.wait());
    });

    select! {
        recv(result_rx) -> result => provider_worker_result_to_value(result),
        recv(cancel_rx) -> _ => {
            if let Ok(result) = result_rx.try_recv() {
                return provider_worker_result_to_value(Ok(result));
            }
            let _ = abort_handle.abort();
            match result_rx.recv() {
                Ok(Ok(message)) => provider_message_to_json_value(message),
                Ok(Err(_)) | Err(_) => Ok(json!({
                    "status": "interrupted",
                    "reason": "tool_interrupted",
                })),
            }
        }
    }
}

fn provider_worker_result_to_value(
    result: Result<Result<ChatMessage, ProviderError>, crossbeam_channel::RecvError>,
) -> Result<Value, LocalToolError> {
    let message = result
        .map_err(|_| LocalToolError::Io("web_search provider worker stopped".to_string()))?
        .map_err(provider_error_to_local_tool_error)?;
    provider_message_to_json_value(message)
}

fn provider_message_to_json_value(message: ChatMessage) -> Result<Value, LocalToolError> {
    let mut text = Vec::new();
    for item in message.data {
        match item {
            ChatMessageItem::Context(context) => text.push(context.text),
            ChatMessageItem::ToolResult(result) => {
                if let Some(structured) = result.result.structured {
                    return Ok(structured);
                }
                let rendered = crate::session_actor::tool_result_text(&result);
                if !rendered.trim().is_empty() {
                    text.push(rendered);
                }
            }
            _ => {}
        }
    }
    let text = text.join("\n");
    serde_json::from_str::<Value>(&text).map_err(|error| {
        LocalToolError::Io(format!(
            "web_search provider returned non-JSON result: {error}"
        ))
    })
}

#[cfg(test)]
fn search_with_provider_direct(
    model_config: &ModelConfig,
    query: &str,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    match model_config.provider_type {
        ProviderType::BraveSearch => BraveSearchProvider::new()
            .search(model_config, query, max_results.clamp(1, 20))
            .map_err(provider_error_to_local_tool_error),
        ProviderType::BraveSearchImage => BraveSearchImageProvider::new()
            .search_images(model_config, query, max_results.clamp(1, 200))
            .map_err(provider_error_to_local_tool_error),
        ProviderType::BraveSearchVideo => BraveSearchVideoProvider::new()
            .search_videos(model_config, query, max_results.clamp(1, 50))
            .map_err(provider_error_to_local_tool_error),
        ProviderType::BraveSearchNews => BraveSearchNewsProvider::new()
            .search_news(model_config, query, max_results.clamp(1, 50))
            .map_err(provider_error_to_local_tool_error),
        _ => Err(LocalToolError::InvalidArguments(format!(
            "unsupported web_search provider {:?}",
            model_config.provider_type
        ))),
    }
}

fn provider_error_to_local_tool_error(error: ProviderError) -> LocalToolError {
    match error {
        ProviderError::MissingApiKeyEnv(env) => LocalToolError::InvalidArguments(format!(
            "missing web search API key in environment variable {env}"
        )),
        error => LocalToolError::Io(format!("web_search provider request failed: {error}")),
    }
}

fn web_search_json_endpoint(
    client: &reqwest::blocking::Client,
    base_url: &str,
    query: &str,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    let mut url = Url::parse(base_url).map_err(|error| {
        LocalToolError::InvalidArguments(format!("invalid web search URL: {error}"))
    })?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("query", query)
        .append_pair("max_results", &max_results.to_string());
    let value = client
        .get(url)
        .send()
        .map_err(|error| LocalToolError::Io(format!("web_search request failed: {error}")))?
        .json::<Value>()
        .map_err(|error| LocalToolError::Io(format!("failed to parse web_search JSON: {error}")))?;
    Ok(value)
}

fn parse_duckduckgo_html_results(body: &str, max_results: usize) -> Vec<Value> {
    let Ok(anchor_regex) =
        Regex::new(r#"(?s)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
    else {
        return Vec::new();
    };
    let snippet_regex =
        Regex::new(r#"(?s)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#).ok();
    let snippets = snippet_regex
        .as_ref()
        .map(|regex| {
            regex
                .captures_iter(body)
                .filter_map(|cap| cap.get(1).map(|value| strip_html(value.as_str())))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    anchor_regex
        .captures_iter(body)
        .take(max_results)
        .enumerate()
        .map(|(index, cap)| {
            let url = cap
                .get(1)
                .map(|value| html_unescape(value.as_str()))
                .unwrap_or_default();
            let title = cap
                .get(2)
                .map(|value| strip_html(value.as_str()))
                .unwrap_or_default();
            json!({
                "title": title,
                "url": normalize_duckduckgo_url(&url),
                "snippet": snippets.get(index).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

fn normalize_duckduckgo_url(url: &str) -> String {
    if let Ok(parsed) = Url::parse(url) {
        if parsed.domain() == Some("duckduckgo.com") {
            if let Some(target) = parsed
                .query_pairs()
                .find_map(|(key, value)| (key == "uddg").then(|| value.to_string()))
            {
                return target;
            }
        }
    }
    url.to_string()
}
