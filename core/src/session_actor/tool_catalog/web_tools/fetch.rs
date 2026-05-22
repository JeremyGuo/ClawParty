use std::{io::Read, str::FromStr, time::Duration};

use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Map, Value};
use url::Url;

use super::common::{
    extract_html_title, extract_links, extract_meta_description, html_unescape, is_html_content,
    is_textual_content_type, looks_like_text, normalize_text_lines, replace_anchors_with_text,
    selected_response_headers,
};
use crate::session_actor::{
    tool_catalog::{
        schema::{object_schema, properties},
        BaseTool, ToolBackend, ToolCallContext, ToolDefinition,
    },
    tool_runtime::{f64_arg_with_default, string_arg, usize_arg_with_default, LocalToolError},
    ToolResultContent,
};

pub(super) struct WebFetchTool;

impl WebFetchTool {
    pub(super) fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "web_fetch",
            "Fetch an HTTP/HTTPS URL and return a structured response. Defaults are timeout_seconds=30, max_chars=20000, method=GET, format=auto. In auto format, HTML is converted to readable markdown-like text with page metadata and links; binary content returns metadata without a lossy body.",
            object_schema(
                properties([
                    ("url", json!({"type": "string", "description": "HTTP or HTTPS URL to fetch."})),
                    ("method", json!({"type": "string", "enum": ["GET", "HEAD"], "description": "HTTP method. Defaults to GET."})),
                    ("timeout_seconds", json!({"type": "number", "minimum": 1, "maximum": 120, "description": "Request timeout in seconds. Defaults to 30."})),
                    ("max_chars", json!({"type": "integer", "minimum": 0, "maximum": 100000, "description": "Maximum response body characters to return. Defaults to 20000."})),
                    ("max_bytes", json!({"type": "integer", "minimum": 0, "maximum": 8388608, "description": "Maximum response body bytes to read before text extraction. Defaults to a bounded value derived from max_chars, capped at 8 MiB."})),
                    ("format", json!({"type": "string", "enum": ["auto", "text", "raw"], "description": "auto strips HTML to readable text, text always strips HTML-like content, raw returns response text unchanged. Defaults to auto."})),
                    ("user_agent", json!({"type": "string", "description": "Optional User-Agent override."})),
                    ("headers", json!({"type": "object", "additionalProperties": {"type": "string"}})),
                ]),
                &["url"],
            ),            ToolBackend::Local,
        )
    }

    pub(super) fn fetch(&self, arguments: &Map<String, Value>) -> Result<Value, LocalToolError> {
        let url = string_arg(arguments, "url")?;
        let parsed_url = Url::parse(&url)
            .map_err(|error| LocalToolError::InvalidArguments(format!("invalid url: {error}")))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(LocalToolError::InvalidArguments(
                "url must use http or https".to_string(),
            ));
        }
        let timeout_seconds = f64_arg_with_default(arguments, "timeout_seconds", 30.0)?;
        if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
            return Err(LocalToolError::InvalidArguments(
                "timeout_seconds must be a positive finite number".to_string(),
            ));
        }
        let timeout_seconds = timeout_seconds.min(120.0);
        let max_chars = usize_arg_with_default(arguments, "max_chars", 20_000)?.min(100_000);
        let default_max_bytes = default_fetch_max_bytes(max_chars);
        let max_bytes = usize_arg_with_default(arguments, "max_bytes", default_max_bytes)?
            .min(MAX_FETCH_BODY_BYTES);
        let format = fetch_format(arguments.get("format"))?;
        let method = fetch_method(arguments.get("method"))?;
        let user_agent = arguments
            .get("user_agent")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("stellaclaw-core/0.1");
        let headers = request_headers(arguments.get("headers"))?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs_f64(timeout_seconds))
            .user_agent(user_agent)
            .build()
            .map_err(|error| LocalToolError::Io(format!("failed to build web client: {error}")))?;
        let response = client
            .request(method.clone(), parsed_url)
            .headers(headers)
            .send()
            .map_err(|error| LocalToolError::Io(format!("web_fetch request failed: {error}")))?;

        fetch_response(response, &url, method, max_bytes, max_chars, format)
    }
}

impl BaseTool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        self.tool_definition()
    }

    fn call(
        &self,
        _ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let arguments = object_arguments(args)?;
        self.fetch(&arguments)
            .map(ToolResultContent::from_tool_value)
    }
}

fn object_arguments(args: Value) -> Result<Map<String, Value>, LocalToolError> {
    let Value::Object(arguments) = args else {
        return Err(LocalToolError::InvalidArguments(
            "tool arguments must be a JSON object".to_string(),
        ));
    };
    Ok(arguments)
}

const MIN_FETCH_BODY_BYTES: usize = 1_048_576;
const MAX_FETCH_BODY_BYTES: usize = 8 * 1_048_576;
const MAX_FETCH_LINKS: usize = 50;

fn fetch_response(
    response: reqwest::blocking::Response,
    requested_url: &str,
    method: reqwest::Method,
    max_bytes: usize,
    max_chars: usize,
    format: RequestedFetchFormat,
) -> Result<Value, LocalToolError> {
    let final_url = response.url().to_string();
    let status = response.status().as_u16();
    let response_headers = response.headers().clone();
    let content_length = response.content_length();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let (raw_body_bytes, body_truncated_by_bytes) =
        read_limited_response_body(response, method, max_bytes, content_length)?;
    let raw_body = String::from_utf8_lossy(&raw_body_bytes).into_owned();
    let body_format =
        resolved_fetch_body_format(format, content_type.as_deref(), &raw_body, &raw_body_bytes);
    let page_metadata = if body_format == FetchBodyFormat::Text
        && is_html_content(content_type.as_deref(), &raw_body)
    {
        PageMetadata::from_html(&raw_body, &final_url)
    } else {
        PageMetadata::default()
    };
    let body = match body_format {
        FetchBodyFormat::Binary => String::new(),
        FetchBodyFormat::Raw => raw_body,
        FetchBodyFormat::Text => html_to_readable_text(&raw_body, content_type.as_deref()),
    };
    let (body, body_truncated_by_chars) = truncate_chars(&body, max_chars);
    let truncated = body_truncated_by_bytes || body_truncated_by_chars;

    Ok(json!({
        "kind": "web_fetch_result",
        "url": requested_url,
        "final_url": final_url,
        "redirected": final_url != requested_url,
        "status": status,
        "ok": (200..300).contains(&status),
        "content_type": content_type,
        "content_length": content_length,
        "body_format": body_format.name(),
        "truncated": truncated,
        "body_truncated_by_bytes": body_truncated_by_bytes,
        "body_truncated_by_chars": body_truncated_by_chars,
        "max_chars": max_chars,
        "max_bytes": max_bytes,
        "bytes_read": raw_body_bytes.len(),
        "title": page_metadata.title,
        "description": page_metadata.description,
        "links": page_metadata.links,
        "headers": selected_response_headers(&response_headers),
        "body": body,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestedFetchFormat {
    Auto,
    Text,
    Raw,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FetchBodyFormat {
    Binary,
    Text,
    Raw,
}

impl FetchBodyFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Text => "text",
            Self::Raw => "raw",
        }
    }
}

fn fetch_format(value: Option<&Value>) -> Result<RequestedFetchFormat, LocalToolError> {
    match value.and_then(Value::as_str).unwrap_or("auto") {
        "auto" => Ok(RequestedFetchFormat::Auto),
        "text" => Ok(RequestedFetchFormat::Text),
        "raw" => Ok(RequestedFetchFormat::Raw),
        other => Err(LocalToolError::InvalidArguments(format!(
            "unsupported web_fetch format {other}"
        ))),
    }
}

fn fetch_method(value: Option<&Value>) -> Result<reqwest::Method, LocalToolError> {
    let method = value.and_then(Value::as_str).unwrap_or("GET");
    match method {
        "GET" | "HEAD" => reqwest::Method::from_str(method).map_err(|error| {
            LocalToolError::InvalidArguments(format!("invalid web_fetch method {method}: {error}"))
        }),
        other => Err(LocalToolError::InvalidArguments(format!(
            "unsupported web_fetch method {other}"
        ))),
    }
}

fn resolved_fetch_body_format(
    requested: RequestedFetchFormat,
    content_type: Option<&str>,
    body: &str,
    body_bytes: &[u8],
) -> FetchBodyFormat {
    match requested {
        RequestedFetchFormat::Raw => FetchBodyFormat::Raw,
        RequestedFetchFormat::Text => FetchBodyFormat::Text,
        RequestedFetchFormat::Auto => {
            if is_html_content(content_type, body) {
                FetchBodyFormat::Text
            } else if is_textual_content_type(content_type) || looks_like_text(body_bytes) {
                FetchBodyFormat::Raw
            } else {
                FetchBodyFormat::Binary
            }
        }
    }
}

fn default_fetch_max_bytes(max_chars: usize) -> usize {
    if max_chars == 0 {
        return 0;
    }
    max_chars
        .saturating_mul(8)
        .max(MIN_FETCH_BODY_BYTES)
        .min(MAX_FETCH_BODY_BYTES)
}

fn read_limited_response_body(
    response: reqwest::blocking::Response,
    method: reqwest::Method,
    max_bytes: usize,
    content_length: Option<u64>,
) -> Result<(Vec<u8>, bool), LocalToolError> {
    if method == reqwest::Method::HEAD || max_bytes == 0 {
        return Ok((Vec::new(), content_length.is_some_and(|length| length > 0)));
    }
    let mut body = Vec::new();
    let limit = max_bytes.saturating_add(1) as u64;
    response
        .take(limit)
        .read_to_end(&mut body)
        .map_err(|error| LocalToolError::Io(format!("failed to read web_fetch body: {error}")))?;
    let truncated = body.len() > max_bytes;
    if truncated {
        body.truncate(max_bytes);
    }
    Ok((body, truncated))
}

#[derive(Default)]
struct PageMetadata {
    title: Option<String>,
    description: Option<String>,
    links: Vec<Value>,
}

impl PageMetadata {
    fn from_html(html: &str, base_url: &str) -> Self {
        Self {
            title: extract_html_title(html),
            description: extract_meta_description(html),
            links: extract_links(html, base_url, MAX_FETCH_LINKS),
        }
    }
}

fn html_to_readable_text(input: &str, content_type: Option<&str>) -> String {
    if !is_html_content(content_type, input) {
        return normalize_text_lines(&html_unescape(input));
    }
    let without_scripts = Regex::new(
        r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<noscript[^>]*>.*?</noscript>",
    )
    .map(|regex| regex.replace_all(input, " ").to_string())
    .unwrap_or_else(|_| input.to_string());
    let without_head = Regex::new(r"(?is)<head[^>]*>.*?</head>")
        .map(|regex| regex.replace_all(&without_scripts, " ").to_string())
        .unwrap_or(without_scripts);
    let with_blocks = Regex::new(
        r"(?is)</?(article|section|main|header|footer|nav|aside|div|p|br|hr|blockquote|pre|table|thead|tbody|tr|h[1-6])[^>]*>",
    )
    .map(|regex| regex.replace_all(&without_head, "\n\n").to_string())
    .unwrap_or(without_head);
    let with_list_items = Regex::new(r"(?is)<li[^>]*>")
        .map(|regex| regex.replace_all(&with_blocks, "\n- ").to_string())
        .unwrap_or(with_blocks);
    let with_link_text = replace_anchors_with_text(&with_list_items);
    let without_tags = Regex::new(r"(?is)<[^>]+>")
        .map(|regex| regex.replace_all(&with_link_text, " ").to_string())
        .unwrap_or(with_link_text);
    normalize_text_lines(&html_unescape(&without_tags))
}

fn request_headers(value: Option<&Value>) -> Result<HeaderMap, LocalToolError> {
    let mut headers = HeaderMap::new();
    let Some(value) = value else {
        return Ok(headers);
    };
    let object = value
        .as_object()
        .ok_or_else(|| LocalToolError::InvalidArguments("headers must be an object".to_string()))?;

    for (name, value) in object {
        let value = value.as_str().ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("header {name} must be a string"))
        })?;
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            LocalToolError::InvalidArguments(format!("invalid header name {name}: {error}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            LocalToolError::InvalidArguments(format!("invalid header value: {error}"))
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let truncated = text.chars().count() > max_chars;
    let body = chars.by_ref().take(max_chars).collect::<String>();
    (body, truncated)
}
