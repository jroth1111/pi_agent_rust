use async_trait::async_trait;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;
use url::Url;

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};

use super::{DEFAULT_WEB_TIMEOUT_SECS, DEFAULT_WEBSEARCH_LIMIT, Tool, ToolOutput, ToolUpdate};

/// Search engine to use.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SearchEngine {
    /// Google direct HTML scraping (default).
    #[default]
    Google,
    /// Bing RSS feed (fallback).
    Bing,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebSearchInput {
    query: String,
    limit: Option<usize>,
    /// Search engine: "google" (default) or "bing".
    engine: Option<SearchEngine>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WebSearchResult {
    title: String,
    url: String,
    domain: String,
    content_type: String,
    snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<String>,
}

/// Extract domain from URL hostname.
fn extract_domain(url_str: &str) -> String {
    Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(std::string::ToString::to_string))
        .unwrap_or_else(|| url_str.to_string())
}

/// Determine content type based on URL patterns.
fn determine_content_type(url_str: &str) -> &'static str {
    let url_lower = url_str.to_lowercase();
    if url_lower.contains("github.com") {
        "github"
    } else if url_lower.contains("stackoverflow.com") || url_lower.contains("stackexchange.com") {
        "forum"
    } else if url_lower.contains("docs.") || url_lower.contains("/docs/") {
        "documentation"
    } else if Path::new(&url_lower)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
    {
        "pdf"
    } else {
        "article"
    }
}

/// Create a WebSearchResult with automatic domain and content_type extraction.
fn create_search_result(
    title: String,
    url: String,
    snippet: String,
    published: Option<String>,
) -> WebSearchResult {
    let domain = extract_domain(&url);
    let content_type = determine_content_type(&url).to_string();
    WebSearchResult {
        title,
        url,
        domain,
        content_type,
        snippet,
        published,
    }
}

pub struct WebSearchTool;

impl WebSearchTool {
    pub const fn new() -> Self {
        Self
    }
}

fn decode_html_entities_minimal(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
}

fn extract_xml_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)?;
    let rest = &block[start + open.len()..];
    let end = rest.find(&close)?;
    Some(rest[..end].trim().to_string())
}

fn parse_bing_rss_results(xml: &str, limit: usize) -> Vec<WebSearchResult> {
    let mut results = Vec::new();
    for item_block in xml.split("<item>").skip(1) {
        let Some((item, _)) = item_block.split_once("</item>") else {
            continue;
        };
        let title = extract_xml_tag(item, "title").unwrap_or_default();
        let url = extract_xml_tag(item, "link").unwrap_or_default();
        let snippet = extract_xml_tag(item, "description").unwrap_or_default();
        let published = extract_xml_tag(item, "pubDate");
        if title.is_empty() || url.is_empty() {
            continue;
        }
        results.push(create_search_result(
            decode_html_entities_minimal(&title),
            decode_html_entities_minimal(&url),
            decode_html_entities_minimal(&snippet),
            published.map(|v| decode_html_entities_minimal(&v)),
        ));
        if results.len() >= limit {
            break;
        }
    }
    results
}

/// Parse Google search HTML results.
/// Selectors based on current Google HTML structure.
fn parse_google_html_results(html: &str, limit: usize) -> Vec<WebSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // Each search result is in a div.g
    let Ok(result_selector) = Selector::parse("div.g") else {
        return results;
    };

    // Link with title (h3 inside)
    let Ok(link_selector) = Selector::parse("a") else {
        return results;
    };

    // Title is usually in an h3
    let Ok(title_selector) = Selector::parse("h3") else {
        return results;
    };

    // Snippet - Google uses various classes, try common ones
    let snippet_selectors: Vec<Selector> =
        ["div[data-sncf]", "span.aCOpRe", "div.VwiC3b", "div.IsZvec"]
            .iter()
            .filter_map(|s| Selector::parse(s).ok())
            .collect();

    for result in document.select(&result_selector) {
        if results.len() >= limit {
            break;
        }

        // Find the main link (first anchor with href)
        let Some(link_el) = result.select(&link_selector).next() else {
            continue;
        };
        let Some(href) = link_el.value().attr("href") else {
            continue;
        };

        // Skip internal Google links and ads
        if href.starts_with('/') || href.is_empty() || href.contains("google.com/search") {
            continue;
        }

        // Extract URL from Google redirect if present
        let url = if href.starts_with("/url?") {
            Url::parse(href)
                .ok()
                .and_then(|u| {
                    u.query_pairs()
                        .find(|(k, _)| k == "q" || k == "url")
                        .map(|(_, v)| v.to_string())
                })
                .unwrap_or_else(|| href.to_string())
        } else {
            href.to_string()
        };

        // Get title from h3
        let title = result
            .select(&title_selector)
            .next()
            .map(|el| el.text().collect::<Vec<_>>().join(""))
            .unwrap_or_default();

        if title.is_empty() {
            continue;
        }

        // Get snippet from various possible selectors
        let snippet = snippet_selectors
            .iter()
            .find_map(|sel| result.select(sel).next())
            .map(|el| el.text().collect::<Vec<_>>().join(""))
            .unwrap_or_default();

        results.push(create_search_result(
            decode_html_entities_minimal(&title),
            decode_html_entities_minimal(&url),
            decode_html_entities_minimal(&snippet),
            None,
        ));
    }

    results
}

/// Check if response looks like a CAPTCHA page.
fn is_captcha_response(html: &str) -> bool {
    html.contains("recaptcha") || html.contains("cf-turnstile") || html.contains("captcha")
}

/// Execute Bing RSS feed search.
async fn execute_bing_search(query: &str, limit: usize) -> Result<(Vec<WebSearchResult>, u16)> {
    let mut search_url = Url::parse("https://www.bing.com/search")
        .map_err(|e| Error::tool("websearch", format!("Failed to build URL: {e}")))?;
    search_url
        .query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "rss");

    let response = crate::http::client::Client::new()
        .get(search_url.as_str())
        .timeout(Duration::from_secs(DEFAULT_WEB_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| Error::tool("websearch", format!("Search request failed: {e}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| Error::tool("websearch", format!("Failed to read response body: {e}")))?;

    let results = if status == 200 {
        parse_bing_rss_results(&body, limit)
    } else {
        Vec::new()
    };

    Ok((results, status))
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "websearch"
    }

    fn label(&self) -> &'static str {
        "websearch"
    }

    fn description(&self) -> &'static str {
        "Search the web and return top results (title, URL, snippet). Default engine is Google (HTML scraping). Use engine=\"bing\" for RSS fallback if Google is blocked."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default 5, max 20)"
                },
                "engine": {
                    "type": "string",
                    "enum": ["google", "bing"],
                    "description": "Search engine: 'google' (default, direct scraping) or 'bing' (RSS feed fallback)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: WebSearchInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let query = input.query.trim();
        if query.is_empty() {
            return Err(Error::validation(
                "websearch.query cannot be empty".to_string(),
            ));
        }

        let limit = input.limit.unwrap_or(DEFAULT_WEBSEARCH_LIMIT).clamp(1, 20);
        let engine = input.engine.unwrap_or_default();

        let (results, engine_used, status, captcha_detected, fallback_used) = match engine {
            SearchEngine::Google => {
                // Google direct HTML scraping
                let mut search_url = Url::parse("https://www.google.com/search")
                    .map_err(|e| Error::tool("websearch", format!("Failed to build URL: {e}")))?;
                search_url.query_pairs_mut().append_pair("q", query);

                // Use realistic User-Agent to avoid immediate blocking
                let response = crate::http::client::Client::new()
                    .get(search_url.as_str())
                    .header(
                        "User-Agent",
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                    )
                    .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
                    .header("Accept-Language", "en-US,en;q=0.5")
                    .timeout(Duration::from_secs(DEFAULT_WEB_TIMEOUT_SECS))
                    .send()
                    .await
                    .map_err(|e| {
                        Error::tool("websearch", format!("Search request failed: {e}"))
                    })?;

                let status = response.status();
                let body = response.text().await.map_err(|e| {
                    Error::tool("websearch", format!("Failed to read response body: {e}"))
                })?;

                let captcha_detected = is_captcha_response(&body);

                if captcha_detected {
                    // Auto-fallback to Bing when CAPTCHA is detected
                    let (bing_results, bing_status) = execute_bing_search(query, limit).await?;
                    (
                        bing_results,
                        "google (fallback to bing)".to_string(),
                        bing_status,
                        true,
                        true,
                    )
                } else {
                    let results = if status == 200 {
                        parse_google_html_results(&body, limit)
                    } else {
                        Vec::new()
                    };
                    (results, "google".to_string(), status, false, false)
                }
            }
            SearchEngine::Bing => {
                let (results, status) = execute_bing_search(query, limit).await?;
                (results, "bing".to_string(), status, false, false)
            }
        };

        let mut out = String::new();
        let _ = writeln!(out, "Search query: {query}");
        let _ = writeln!(out, "Engine: {engine_used}");
        let _ = writeln!(out, "HTTP status: {status}");

        if fallback_used {
            out.push_str("\n[i] Google CAPTCHA detected, automatically fell back to Bing.");
        }

        if results.is_empty() {
            out.push_str("\nNo results found.");
        } else {
            let _ = writeln!(out, "\nTop {} result(s):", results.len());
            for (i, result) in results.iter().enumerate() {
                let _ = writeln!(out, "{}. {}", i + 1, result.title);
                let _ = writeln!(out, "   URL: {} ({})", result.url, result.domain);
                if !result.snippet.is_empty() {
                    let _ = writeln!(out, "   Snippet: {}", result.snippet);
                }
                if let Some(date) = &result.published {
                    let _ = writeln!(out, "   Published: {date}");
                }
            }
        }

        let details = serde_json::json!({
            "query": query,
            "engine": engine_used,
            "status": status,
            "captchaDetected": captcha_detected,
            "fallbackUsed": fallback_used,
            "resultCount": results.len(),
            "results": results,
        });

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(out))],
            details: Some(details),
            is_error: status >= 400,
        })
    }
}
