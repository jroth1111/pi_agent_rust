use async_trait::async_trait;
use serde::Deserialize;
use std::fmt::Write as _;
use std::time::Duration;
use url::Url;

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};

use super::html_extract;
use super::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, DEFAULT_WEB_TIMEOUT_SECS, Tool, ToolOutput, ToolUpdate,
    TruncationResult, format_size, truncate_head,
};

/// Extraction mode for HTML content.
#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum ExtractMode {
    /// Smart extraction using semantic HTML selectors (default).
    #[default]
    Smart,
    /// Readability-style extraction with content density scoring.
    Readability,
    /// Convert HTML to Markdown format.
    Markdown,
    /// Legacy regex-based extraction.
    Legacy,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebFetchInput {
    url: String,
    timeout: Option<u64>,
    raw: Option<bool>,
    /// Extraction mode: "smart" (default), "readability", "markdown", or "legacy".
    extract: Option<ExtractMode>,
}

#[derive(Debug)]
struct WebFetchResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    final_url: String,
    redirects: usize,
}

pub struct WebFetchTool;

impl WebFetchTool {
    pub const fn new() -> Self {
        Self
    }
}

const fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

async fn fetch_url_following_redirects(
    client: &crate::http::client::Client,
    url: &str,
    timeout_secs: Option<u64>,
    max_redirects: usize,
) -> Result<WebFetchResponse> {
    let mut current_url = url.to_string();
    for hop in 0..=max_redirects {
        let mut request = client.get(&current_url).header(
            "Accept",
            "text/html,application/json,text/plain;q=0.9,*/*;q=0.8",
        );
        if let Some(timeout) = timeout_secs {
            request = request.timeout(Duration::from_secs(timeout));
        } else {
            request = request.no_timeout();
        }

        let response = request.send().await.map_err(|e| {
            Error::tool("webfetch", format!("Request to {current_url} failed: {e}"))
        })?;
        let status = response.status();
        let headers = response.headers().to_vec();
        if is_redirect_status(status) {
            if hop >= max_redirects {
                return Err(Error::tool(
                    "webfetch",
                    format!("Too many redirects (>{max_redirects})"),
                ));
            }
            if let Some(location) = header_value(&headers, "Location") {
                let next = Url::parse(&current_url)
                    .and_then(|base| base.join(&location))
                    .map_err(|e| {
                        Error::tool("webfetch", format!("Invalid redirect location: {e}"))
                    })?;
                current_url = next.to_string();
                continue;
            }
        }

        let body = response
            .text()
            .await
            .map_err(|e| Error::tool("webfetch", format!("Failed to decode response body: {e}")))?;

        return Ok(WebFetchResponse {
            status,
            headers,
            body,
            final_url: current_url,
            redirects: hop,
        });
    }
    Err(Error::tool(
        "webfetch",
        "Unexpected redirect loop".to_string(),
    ))
}

fn append_truncation_notice(text: &mut String, truncation: &TruncationResult) {
    if truncation.truncated {
        let _ = write!(
            text,
            "\n\n[Truncated: showing {}/{} lines, {}/{} bytes]",
            truncation.output_lines,
            truncation.total_lines,
            format_size(truncation.output_bytes),
            format_size(truncation.total_bytes)
        );
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "webfetch"
    }

    fn label(&self) -> &'static str {
        "webfetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a URL and return readable text content. Follows redirects up to 5 hops. Use raw=true to return body as-is. Use extract=\"readability\" for content-density extraction or extract=\"markdown\" for Markdown output."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "HTTP or HTTPS URL to fetch"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 20; set 0 to disable)"
                },
                "raw": {
                    "type": "boolean",
                    "description": "Return raw body instead of HTML-to-text conversion"
                },
                "extract": {
                    "type": "string",
                    "enum": ["smart", "readability", "markdown", "legacy"],
                    "description": "HTML extraction mode: 'smart' (default) uses DOM selectors, 'readability' uses content density scoring, 'markdown' converts to Markdown, 'legacy' uses regex"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: WebFetchInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let url = input.url.trim();
        if url.is_empty() {
            return Err(Error::validation(
                "webfetch.url cannot be empty".to_string(),
            ));
        }

        let parsed_url =
            Url::parse(url).map_err(|e| Error::validation(format!("Invalid URL: {e}")))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(Error::validation(
                "webfetch.url must use http or https".to_string(),
            ));
        }

        let timeout_secs = match input.timeout {
            Some(0) => None,
            Some(value) => Some(value),
            None => Some(DEFAULT_WEB_TIMEOUT_SECS),
        };

        let response = fetch_url_following_redirects(
            &crate::http::client::Client::new(),
            url,
            timeout_secs,
            5,
        )
        .await?;

        let content_type = header_value(&response.headers, "content-type").unwrap_or_default();
        let raw = input.raw.unwrap_or(false);
        let extract_mode = input.extract.unwrap_or_default();

        let (text, extraction_method, compression_ratio) = if raw {
            (response.body.clone(), None, None)
        } else if content_type.to_ascii_lowercase().contains("text/html")
            || response.body.contains("<html")
        {
            match extract_mode {
                ExtractMode::Smart => {
                    let extracted = html_extract::extract_content(&response.body);
                    (
                        extracted.body,
                        Some(extracted.extraction_method),
                        Some(extracted.compression_ratio),
                    )
                }
                ExtractMode::Readability => {
                    let extracted = html_extract::extract_readability(&response.body);
                    (
                        extracted.body,
                        Some(extracted.extraction_method),
                        Some(extracted.compression_ratio),
                    )
                }
                ExtractMode::Markdown => {
                    let markdown = html_extract::html_to_markdown(&response.body);
                    (markdown, Some("markdown".to_string()), None)
                }
                ExtractMode::Legacy => {
                    let text = html_extract::html_to_text_legacy(&response.body);
                    (text, Some("legacy".to_string()), None)
                }
            }
        } else {
            (response.body.clone(), None, None)
        };

        let truncation = truncate_head(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut text = truncation.content.clone();
        append_truncation_notice(&mut text, &truncation);

        let details = serde_json::json!({
            "url": url,
            "finalUrl": response.final_url,
            "status": response.status,
            "contentType": content_type,
            "redirects": response.redirects,
            "truncation": truncation,
            "extractionMethod": extraction_method,
            "compressionRatio": compression_ratio,
        });

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(details),
            is_error: response.status >= 400,
        })
    }
}
