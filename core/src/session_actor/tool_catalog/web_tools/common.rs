use regex::Regex;
use reqwest::header::HeaderMap;
use serde_json::{json, Map, Value};
use url::Url;

pub(super) fn is_html_content(content_type: Option<&str>, body: &str) -> bool {
    content_type.is_some_and(|value| value.to_ascii_lowercase().contains("html"))
        || body
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("<!doctype")
        || body.trim_start().to_ascii_lowercase().starts_with("<html")
}

pub(super) fn is_textual_content_type(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let content_type = content_type.to_ascii_lowercase();
    content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.contains("ecmascript")
        || content_type.contains("x-www-form-urlencoded")
        || content_type.contains("csv")
        || content_type.contains("yaml")
        || content_type.contains("svg")
}

pub(super) fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if std::str::from_utf8(bytes).is_ok() {
        return true;
    }
    if bytes.iter().any(|byte| *byte == 0) {
        return false;
    }
    let control = bytes
        .iter()
        .filter(|byte| matches!(byte, 0..=8 | 11 | 12 | 14..=31))
        .count();
    control.saturating_mul(100) / bytes.len() < 2
}

pub(super) fn selected_response_headers(headers: &HeaderMap) -> Map<String, Value> {
    let mut selected = Map::new();
    for name in [
        reqwest::header::CACHE_CONTROL,
        reqwest::header::CONTENT_DISPOSITION,
        reqwest::header::ETAG,
        reqwest::header::LAST_MODIFIED,
        reqwest::header::LOCATION,
    ] {
        if let Some(value) = headers.get(&name).and_then(|value| value.to_str().ok()) {
            selected.insert(name.as_str().to_string(), Value::String(value.to_string()));
        }
    }
    selected
}

pub(super) fn extract_html_title(html: &str) -> Option<String> {
    Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
        .ok()
        .and_then(|regex| regex.captures(html))
        .and_then(|captures| captures.get(1).map(|value| strip_html(value.as_str())))
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn extract_meta_description(html: &str) -> Option<String> {
    for pattern in [
        r#"(?is)<meta[^>]*(?:name|property)\s*=\s*["'](?:description|og:description|twitter:description)["'][^>]*content\s*=\s*["']([^"']*)["'][^>]*>"#,
        r#"(?is)<meta[^>]*content\s*=\s*["']([^"']*)["'][^>]*(?:name|property)\s*=\s*["'](?:description|og:description|twitter:description)["'][^>]*>"#,
    ] {
        if let Some(description) = Regex::new(pattern)
            .ok()
            .and_then(|regex| regex.captures(html))
            .and_then(|captures| captures.get(1).map(|value| html_unescape(value.as_str())))
            .map(|value| normalize_text_lines(&value))
            .filter(|value| !value.is_empty())
        {
            return Some(description);
        }
    }
    None
}

pub(super) fn extract_links(html: &str, base_url: &str, max_links: usize) -> Vec<Value> {
    let Some(anchor_regex) =
        Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))[^>]*>(.*?)</a>"#)
            .ok()
    else {
        return Vec::new();
    };
    let base = Url::parse(base_url).ok();
    anchor_regex
        .captures_iter(html)
        .filter_map(|captures| {
            let href = captures
                .get(1)
                .or_else(|| captures.get(2))
                .or_else(|| captures.get(3))?
                .as_str()
                .trim();
            let url = normalize_page_link(href, base.as_ref())?;
            let text = captures
                .get(4)
                .map(|value| strip_html(value.as_str()))
                .unwrap_or_default();
            (!text.is_empty() || !url.is_empty()).then(|| {
                json!({
                    "text": text,
                    "url": url,
                })
            })
        })
        .take(max_links)
        .collect()
}

pub(super) fn normalize_page_link(href: &str, base_url: Option<&Url>) -> Option<String> {
    let href = html_unescape(href);
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let parsed = Url::parse(href)
        .ok()
        .or_else(|| base_url.and_then(|base| base.join(href).ok()))?;
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.to_string())
}

pub(super) fn replace_anchors_with_text(html: &str) -> String {
    let Some(anchor_regex) =
        Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))[^>]*>(.*?)</a>"#)
            .ok()
    else {
        return html.to_string();
    };
    anchor_regex
        .replace_all(html, |captures: &regex::Captures<'_>| {
            let text = captures
                .get(4)
                .map(|value| strip_html(value.as_str()))
                .unwrap_or_default();
            if text.is_empty() {
                captures
                    .get(0)
                    .map(|value| value.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                text
            }
        })
        .to_string()
}

pub(super) fn strip_html(input: &str) -> String {
    let without_tags = Regex::new(r"<[^>]+>")
        .map(|regex| regex.replace_all(input, "").to_string())
        .unwrap_or_else(|_| input.to_string());
    html_unescape(&without_tags)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn html_unescape(input: &str) -> String {
    let named = input
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    let decoded = Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);")
        .map(|regex| {
            regex
                .replace_all(&named, |captures: &regex::Captures<'_>| {
                    let raw = captures.get(1).map(|value| value.as_str()).unwrap_or("");
                    let parsed = raw
                        .strip_prefix('x')
                        .or_else(|| raw.strip_prefix('X'))
                        .map(|hex| u32::from_str_radix(hex, 16))
                        .unwrap_or_else(|| raw.parse::<u32>());
                    parsed
                        .ok()
                        .and_then(char::from_u32)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| {
                            captures
                                .get(0)
                                .map(|value| value.as_str())
                                .unwrap_or("")
                                .to_string()
                        })
                })
                .to_string()
        })
        .unwrap_or(named);
    decoded
}

pub(super) fn normalize_text_lines(input: &str) -> String {
    let mut lines = Vec::new();
    let mut previous_blank = true;
    for raw_line in input.lines() {
        let line = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if !previous_blank {
                lines.push(String::new());
            }
            previous_blank = true;
            continue;
        }
        lines.push(line);
        previous_blank = false;
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}
