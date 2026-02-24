//! HTML content extraction combining DOM parsing with thorough text processing.
//!
//! Uses intelligent CSS selectors to find main content, then applies comprehensive
//! text cleanup including script/style removal and entity decoding.
//! Also includes Mozilla Readability-style extraction using content density scoring.

use regex::Regex;
use scraper::{Html, Selector};
use std::sync::OnceLock;

/// Result of content extraction.
#[derive(Debug, Clone)]
pub struct ExtractedContent {
    /// Page title from <title> element.
    pub title: Option<String>,
    /// Extracted body text, normalized.
    pub body: String,
    /// Compression ratio (body bytes / original HTML bytes).
    pub compression_ratio: f64,
    /// Selector that matched (e.g., "article", "main", "body").
    pub extraction_method: String,
    /// Author from meta author tag.
    pub author: Option<String>,
    /// Publication date from meta published/date tags.
    pub published: Option<String>,
}

/// Extract main content from HTML.
///
/// Combines DOM-based content detection with thorough text processing:
/// 1. Uses CSS selectors to find the main content area (article, main, etc.)
/// 2. Strips script/style tags from the selected content
/// 3. Removes remaining HTML tags
/// 4. Decodes HTML entities
/// 5. Normalizes whitespace
pub fn extract_content(html: &str) -> ExtractedContent {
    let document = Html::parse_document(html);

    // Extract title
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|node| node.text().collect::<Vec<_>>().join(" "))
        .map(|t| normalize_text(&t));

    // Selector fallback order - try semantic elements first
    let selectors = [
        "article",
        "main",
        "div#content",
        "div.article",
        "div.post",
        "div.entry-content",
        "section",
        "body",
    ];

    let mut extracted_html = None;
    let mut method = "body".to_string();

    for selector in &selectors {
        if let Ok(sel) = Selector::parse(selector) {
            if let Some(node) = document.select(&sel).next() {
                // Get the HTML of this node, not just text
                let html_fragment = node.html();
                if !html_fragment.trim().is_empty() {
                    extracted_html = Some(html_fragment);
                    method = (*selector).to_string();
                    break;
                }
            }
        }
    }

    // Fallback to full document
    let html_content = extracted_html.unwrap_or_else(|| document.html());

    // Now apply thorough text processing
    let body = html_to_text(&html_content);

    let ratio = if html.is_empty() {
        0.0
    } else {
        let body_len = u32::try_from(body.len()).unwrap_or(u32::MAX);
        let html_len = u32::try_from(html.len().max(1)).unwrap_or(u32::MAX);
        f64::from(body_len) / f64::from(html_len)
    };

    // Extract metadata
    let author = extract_meta_content(&document, "author");
    let published = extract_meta_date(&document);

    ExtractedContent {
        title,
        body,
        compression_ratio: ratio,
        extraction_method: method,
        author,
        published,
    }
}

/// Convert HTML to clean text with thorough processing.
///
/// Steps:
/// 1. Remove script and style blocks
/// 2. Convert block elements to newlines
/// 3. Remove remaining tags
/// 4. Decode HTML entities
/// 5. Normalize whitespace
fn html_to_text(html: &str) -> String {
    static SCRIPT_STYLE_RE: OnceLock<Regex> = OnceLock::new();
    static BLOCK_TAGS_RE: OnceLock<Regex> = OnceLock::new();
    static TAG_RE: OnceLock<Regex> = OnceLock::new();

    let script_style = SCRIPT_STYLE_RE.get_or_init(|| {
        Regex::new(
            r"(?is)<(?:script|style|noscript|iframe|svg|nav|footer|header)[^>]*>.*?</(?:script|style|noscript|iframe|svg|nav|footer|header)>",
        )
            .expect("valid script/style stripping regex")
    });

    let block_tags = BLOCK_TAGS_RE.get_or_init(|| {
        Regex::new(r"(?is)</?(p|div|br|li|tr|td|th|h[1-6]|article|section|main|aside)[^>]*>")
            .expect("valid block tag regex")
    });

    let tags = TAG_RE.get_or_init(|| Regex::new(r"(?is)<[^>]+>").expect("valid tag regex"));

    // 1. Remove script, style, and other non-content blocks
    let without_code = script_style.replace_all(html, " ");

    // 2. Convert block elements to newlines (preserves structure)
    let with_breaks = block_tags.replace_all(&without_code, "\n");

    // 3. Remove remaining tags
    let no_tags = tags.replace_all(&with_breaks, " ");

    // 4. Decode HTML entities
    let decoded = decode_html_entities(&no_tags);

    // 5. Normalize whitespace
    normalize_text(&decoded)
}

/// Normalize whitespace: collapse multiple spaces/newlines, trim.
fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_newline = false;

    for line in text.lines() {
        let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");

        if line.is_empty() {
            if !prev_newline && !out.is_empty() {
                out.push('\n');
                prev_newline = true;
            }
        } else {
            if !out.is_empty() && !prev_newline {
                out.push('\n');
            }
            out.push_str(&line);
            prev_newline = false;
        }
    }

    out.trim().to_string()
}

/// Decode common HTML entities.
fn decode_html_entities(input: &str) -> String {
    static ENTITY_RE: OnceLock<Regex> = OnceLock::new();

    let entity_re = ENTITY_RE.get_or_init(|| {
        Regex::new(r"&(?:#(\d+)|#x([0-9a-fA-F]+)|(\w+));").expect("valid entity regex")
    });

    let result = entity_re.replace_all(input, |caps: &regex::Captures| {
        // Numeric entity (decimal)
        if let Some(num) = caps.get(1) {
            if let Ok(code) = num.as_str().parse::<u32>() {
                if let Some(ch) = char::from_u32(code) {
                    return ch.to_string();
                }
            }
        }
        // Numeric entity (hex)
        if let Some(hex) = caps.get(2) {
            if let Ok(code) = u32::from_str_radix(hex.as_str(), 16) {
                if let Some(ch) = char::from_u32(code) {
                    return ch.to_string();
                }
            }
        }
        // Named entity
        if let Some(name) = caps.get(3) {
            match name.as_str() {
                "quot" | "ldquo" | "rdquo" => return "\"".to_string(),
                "apos" | "lsquo" | "rsquo" => return "'".to_string(),
                "lt" => return "<".to_string(),
                "gt" => return ">".to_string(),
                "amp" => return "&".to_string(),
                "nbsp" => return " ".to_string(),
                "mdash" | "ndash" => return "—".to_string(),
                "hellip" => return "…".to_string(),
                _ => {}
            }
        }
        // Unknown entity - keep as-is
        caps.get(0).unwrap().as_str().to_string()
    });

    result.into_owned()
}

/// Extract meta tag content by name.
fn extract_meta_content(document: &Html, name: &str) -> Option<String> {
    // Try <meta name="...">
    let selector = Selector::parse(&format!("meta[name=\"{name}\"]")).ok()?;
    if let Some(node) = document.select(&selector).next() {
        if let Some(content) = node.value().attr("content") {
            return Some(normalize_text(content));
        }
    }

    // Try <meta property="article:author"> etc.
    let prop_selector = Selector::parse(&format!("meta[property*=\"{name}\"]")).ok()?;
    if let Some(node) = document.select(&prop_selector).next() {
        if let Some(content) = node.value().attr("content") {
            return Some(normalize_text(content));
        }
    }

    None
}

/// Extract publication date from various meta tags.
fn extract_meta_date(document: &Html) -> Option<String> {
    let date_selectors = [
        "meta[property=\"article:published_time\"]",
        "meta[name=\"date\"]",
        "meta[name=\"publishdate\"]",
        "meta[name=\"published\"]",
        "meta[property=\"datePublished\"]",
        "time[datetime]",
    ];

    for selector_str in &date_selectors {
        if let Ok(selector) = Selector::parse(selector_str) {
            if let Some(node) = document.select(&selector).next() {
                if let Some(date) = node
                    .value()
                    .attr("content")
                    .or_else(|| node.value().attr("datetime"))
                {
                    if !date.is_empty() {
                        return Some(date.to_string());
                    }
                }
            }
        }
    }

    None
}

/// Readability-style extraction using content density scoring.
///
/// This algorithm scores elements by their text density and content indicators,
/// similar to Mozilla's Readability. It finds the most likely main content container
/// and extracts text from it, filtering out navigation, ads, and sidebars.
pub fn extract_readability(html: &str) -> ExtractedContent {
    let document = Html::parse_document(html);

    // Extract title
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|node| node.text().collect::<Vec<_>>().join(" "))
        .map(|t| normalize_text(&t));

    // Extract metadata
    let author = extract_meta_content(&document, "author");
    let published = extract_meta_date(&document);

    // Candidate selectors for main content
    let candidates = [
        "article",
        "main",
        "div.content",
        "div.post",
        "div.article",
        "section",
    ];

    let mut best_score = 0.0;
    let mut best_content = None;
    let mut best_method = "readability".to_string();

    for selector_str in &candidates {
        if let Ok(selector) = Selector::parse(selector_str) {
            for node in document.select(&selector) {
                let score = score_element(&node);
                if score > best_score {
                    best_score = score;
                    best_content = Some(node.html());
                    best_method = (*selector_str).to_string();
                }
            }
        }
    }

    // If no good candidate found, fall back to body
    let html_content = best_content.unwrap_or_else(|| {
        if let Ok(body_sel) = Selector::parse("body") {
            if let Some(body) = document.select(&body_sel).next() {
                body.html()
            } else {
                document.html()
            }
        } else {
            document.html()
        }
    });

    let body = html_to_text(&html_content);

    let ratio = if html.is_empty() {
        0.0
    } else {
        let body_len = u32::try_from(body.len()).unwrap_or(u32::MAX);
        let html_len = u32::try_from(html.len().max(1)).unwrap_or(u32::MAX);
        f64::from(body_len) / f64::from(html_len)
    };

    ExtractedContent {
        title,
        body,
        compression_ratio: ratio,
        extraction_method: format!("readability:{best_method}"),
        author,
        published,
    }
}

/// Score an element based on content density indicators.
fn score_element(element: &scraper::element_ref::ElementRef) -> f64 {
    let text: String = element.text().collect();
    let text_len = text.len() as f64;

    // Base score from text length
    let mut score = text_len;

    // Bonus for paragraph content
    if let Ok(p_sel) = Selector::parse("p") {
        let p_count = element.select(&p_sel).count() as f64;
        score += p_count * 20.0;
    }

    // Bonus for commas (indicates real content)
    let comma_count = text.matches(',').count() as f64;
    score += comma_count * 10.0;

    // Penalty for nav/footer/aside elements
    if let Ok(nav_sel) =
        Selector::parse("nav, footer, aside, .sidebar, .nav, .footer, .advertisement, .ad")
    {
        let bad_count = element.select(&nav_sel).count() as f64;
        score -= bad_count * 50.0;
    }

    // Penalty for too many links (navigation-heavy)
    if let Ok(a_sel) = Selector::parse("a") {
        let link_count = element.select(&a_sel).count() as f64;
        let link_density = if text_len > 0.0 {
            link_count / text_len * 1000.0
        } else {
            0.0
        };
        score -= link_density * 5.0;
    }

    score.max(0.0)
}

/// Convert HTML to Markdown format.
///
/// Preserves document structure for better LLM understanding:
/// - h1-h6 → #, ##
/// - p → paragraph with blank lines
/// - ul/ol → - or 1.
/// - a → [text](url)
/// - img → ![alt](src)
/// - code → `code`
/// - pre/code → ```\ncode\n```
/// - blockquote → > text
pub fn html_to_markdown(html: &str) -> String {
    let document = Html::parse_document(html);
    let mut output = String::new();

    // Extract title as h1
    if let Ok(title_sel) = Selector::parse("title") {
        if let Some(title_node) = document.select(&title_sel).next() {
            let title_text: String = title_node.text().collect();
            let title_text = title_text.trim();
            if !title_text.is_empty() {
                output.push_str("# ");
                output.push_str(title_text);
                output.push_str("\n\n");
            }
        }
    }

    // Find main content area
    let content_selectors = ["article", "main", "div.content", "div.post", "body"];
    let mut content_node = None;

    for sel_str in &content_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(node) = document.select(&sel).next() {
                content_node = Some(node);
                break;
            }
        }
    }

    let content = content_node.map_or(document.root_element(), |n| n);

    // Process child nodes
    process_node_to_markdown(content, &mut output, 0);

    // Clean up extra blank lines
    let mut result = String::new();
    let mut prev_blank = false;
    for line in output.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && prev_blank {
            continue;
        }
        prev_blank = is_blank;
        result.push_str(line);
        result.push('\n');
    }

    result.trim().to_string()
}

/// Process a DOM node and convert to Markdown.
fn process_node_to_markdown(
    node: scraper::element_ref::ElementRef,
    output: &mut String,
    list_depth: usize,
) {
    for child in node.children() {
        match child.value() {
            scraper::Node::Element(_) => {
                let Some(child_el) = scraper::element_ref::ElementRef::wrap(child) else {
                    continue;
                };
                let tag = child_el.value().name();

                match tag {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = tag[1..].parse::<usize>().unwrap_or(1);
                        let text: String = child_el.text().collect();
                        let text = text.trim();
                        if !text.is_empty() {
                            output.push_str(&"#".repeat(level));
                            output.push(' ');
                            output.push_str(text);
                            output.push_str("\n\n");
                        }
                    }
                    "p" => {
                        let text: String = child_el.text().collect();
                        let text = text.trim();
                        if !text.is_empty() {
                            output.push_str(text);
                            output.push_str("\n\n");
                        }
                    }
                    "ul" | "ol" => {
                        process_list_to_markdown_simple(child_el, output, tag == "ol");
                        output.push('\n');
                    }
                    "blockquote" => {
                        let text: String = child_el.text().collect();
                        for line in text.lines() {
                            output.push_str("> ");
                            output.push_str(line.trim());
                            output.push('\n');
                        }
                        output.push('\n');
                    }
                    "pre" => {
                        let code_text: String = child_el.text().collect();
                        output.push_str("```\n");
                        output.push_str(code_text.trim());
                        output.push_str("\n```\n\n");
                    }
                    "code" => {
                        // Inline code (if not inside pre)
                        let text: String = child_el.text().collect();
                        output.push('`');
                        output.push_str(text.trim());
                        output.push('`');
                    }
                    "a" => {
                        let text: String = child_el.text().collect();
                        let href = child_el.value().attr("href").unwrap_or("");
                        if !text.trim().is_empty() && !href.is_empty() {
                            output.push('[');
                            output.push_str(text.trim());
                            output.push_str("](");
                            output.push_str(href);
                            output.push(')');
                        } else {
                            output.push_str(&text);
                        }
                    }
                    "img" => {
                        let alt = child_el.value().attr("alt").unwrap_or("");
                        let src = child_el.value().attr("src").unwrap_or("");
                        output.push_str("![");
                        output.push_str(alt);
                        output.push_str("](");
                        output.push_str(src);
                        output.push(')');
                    }
                    "br" => {
                        output.push_str("  \n");
                    }
                    "hr" => {
                        output.push_str("\n---\n\n");
                    }
                    "strong" | "b" => {
                        let text: String = child_el.text().collect();
                        output.push_str("**");
                        output.push_str(&text);
                        output.push_str("**");
                    }
                    "em" | "i" => {
                        let text: String = child_el.text().collect();
                        output.push('*');
                        output.push_str(&text);
                        output.push('*');
                    }
                    "div" | "section" | "article" | "main" | "span" => {
                        // Recursively process container elements
                        process_node_to_markdown(child_el, output, list_depth);
                    }
                    _ => {
                        // For other elements, just extract text
                        let text: String = child_el.text().collect();
                        output.push_str(&text);
                    }
                }
            }
            scraper::Node::Text(text) => {
                let text = text.text.trim();
                if !text.is_empty() {
                    output.push_str(text);
                }
            }
            _ => {}
        }
    }
}

/// Process inline content (within paragraphs) to Markdown.
fn inline_text_to_markdown(node: scraper::element_ref::ElementRef) -> String {
    let mut result = String::new();

    for child in node.children() {
        match child.value() {
            scraper::Node::Text(text) => {
                result.push_str(text.text.trim());
                result.push(' ');
            }
            scraper::Node::Element(_) => {
                let Some(child_el) = scraper::element_ref::ElementRef::wrap(child) else {
                    continue;
                };
                let tag = child_el.value().name();
                let text: String = child_el.text().collect();
                match tag {
                    "strong" | "b" => {
                        result.push_str("**");
                        result.push_str(text.trim());
                        result.push_str("** ");
                    }
                    "em" | "i" => {
                        result.push('*');
                        result.push_str(text.trim());
                        result.push_str("* ");
                    }
                    "code" => {
                        result.push('`');
                        result.push_str(text.trim());
                        result.push_str("` ");
                    }
                    "a" => {
                        let href = child_el.value().attr("href").unwrap_or("");
                        if !text.trim().is_empty() && !href.is_empty() {
                            result.push('[');
                            result.push_str(text.trim());
                            result.push_str("](");
                            result.push_str(href);
                            result.push_str(") ");
                        } else {
                            result.push_str(&text);
                            result.push(' ');
                        }
                    }
                    "br" => {
                        result.push_str("  \n");
                    }
                    _ => {
                        result.push_str(text.trim());
                        result.push(' ');
                    }
                }
            }
            _ => {}
        }
    }

    result.trim().to_string()
}

/// Process list elements to Markdown (simple version using NodeRef).
fn process_list_to_markdown_simple(
    node: scraper::element_ref::ElementRef,
    output: &mut String,
    ordered: bool,
) {
    let indent = "";
    let mut item_num = 1;

    for child in node.children() {
        if let Some(child_el) = scraper::element_ref::ElementRef::wrap(child) {
            if child_el.value().name() == "li" {
                output.push_str(indent);
                if ordered {
                    output.push_str(&format!("{item_num}. "));
                    item_num += 1;
                } else {
                    output.push_str("- ");
                }

                // Get list item text
                let text: String = child_el.text().collect();
                output.push_str(text.trim());
                output.push('\n');
            }
        }
    }
}

/// Process list elements to Markdown.
fn process_list_to_markdown(
    node: scraper::element_ref::ElementRef,
    output: &mut String,
    ordered: bool,
    depth: usize,
) {
    let indent = "  ".repeat(depth.saturating_sub(1));
    let mut item_num = 1;

    for child in node.children() {
        if let Some(child_el) = scraper::element_ref::ElementRef::wrap(child) {
            if child_el.value().name() == "li" {
                output.push_str(&indent);
                if ordered {
                    output.push_str(&format!("{item_num}. "));
                    item_num += 1;
                } else {
                    output.push_str("- ");
                }

                // Get list item text
                let text: String = child_el.text().collect();
                output.push_str(text.trim());
                output.push('\n');
            }
        }
    }
}

/// Legacy HTML-to-text conversion using regex (no DOM parsing).
///
/// This is the original implementation kept for backwards compatibility
/// when users specify `extract: "legacy"`.
pub fn html_to_text_legacy(html: &str) -> String {
    static SCRIPT_STYLE_RE: OnceLock<Regex> = OnceLock::new();
    static TAG_RE: OnceLock<Regex> = OnceLock::new();

    let script_style = SCRIPT_STYLE_RE.get_or_init(|| {
        Regex::new(r"(?is)<(script|style)[^>]*>.*?</(script|style)>")
            .expect("valid script/style stripping regex")
    });
    let tags = TAG_RE.get_or_init(|| Regex::new(r"(?is)<[^>]+>").expect("valid tag regex"));

    let without_code = script_style.replace_all(html, " ");
    let no_tags = tags.replace_all(&without_code, "\n");
    let decoded = decode_html_entities(&no_tags);

    normalize_text(&decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_article() {
        let html = r"
            <html>
            <head><title>Test Page</title></head>
            <body>
                <nav>Navigation here</nav>
                <article>
                    <h1>Main Article</h1>
                    <p>This is the main content of the article.</p>
                </article>
                <footer>Footer content</footer>
            </body>
            </html>
        ";

        let result = extract_content(html);
        assert_eq!(result.title, Some("Test Page".to_string()));
        assert_eq!(result.extraction_method, "article");
        assert!(result.body.contains("Main Article"));
        // nav/footer are outside article, so they shouldn't appear
        assert!(!result.body.contains("Navigation"));
    }

    #[test]
    fn test_script_removal() {
        let html =
            r"<html><body><p>Hello</p><script>alert('xss')</script><p>World</p></body></html>";
        let result = extract_content(html);
        assert!(result.body.contains("Hello"));
        assert!(result.body.contains("World"));
        assert!(!result.body.contains("alert"));
        assert!(!result.body.contains("script"));
    }

    #[test]
    fn test_entity_decoding() {
        let html = r"<html><body><p>Hello &quot;World&quot; &amp; friends &lt;3</p></body></html>";
        let result = extract_content(html);
        assert!(result.body.contains("Hello \"World\" & friends <3"));
    }

    #[test]
    fn test_numeric_entities() {
        let html = r"<html><body><p>&#72;&#101;&#108;&#108;&#111;</p></body></html>";
        let result = extract_content(html);
        assert!(result.body.contains("Hello"));
    }

    #[test]
    fn test_block_structure() {
        let html = r"<html><body><h1>Title</h1><p>Para 1</p><p>Para 2</p></body></html>";
        let result = extract_content(html);
        assert!(result.body.contains("Title"));
        assert!(result.body.contains("Para 1"));
        assert!(result.body.contains("Para 2"));
    }

    #[test]
    fn test_compression_ratio() {
        let html = "<html><body>".repeat(10) + "Hello World";
        let result = extract_content(&html);
        assert!(result.compression_ratio < 1.0);
    }
}
