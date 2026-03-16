use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};

use super::{Tool, ToolOutput, ToolUpdate, resolve_path};

const DEFAULT_RESULT_LIMIT: usize = 20;
const MAX_RESULT_LIMIT: usize = 200;
const MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeQueryInput {
    query: Option<String>,
    path: Option<String>,
    symbol: Option<String>,
    limit: Option<usize>,
    before: Option<usize>,
    after: Option<usize>,
}

#[derive(Debug, Clone)]
struct SymbolMatch {
    path: PathBuf,
    line: usize,
    kind: String,
    name: String,
    line_text: String,
}

pub struct CodeQueryTool {
    cwd: PathBuf,
}

pub struct CodeContextTool {
    cwd: PathBuf,
}

pub struct CodeImpactTool {
    cwd: PathBuf,
}

impl CodeQueryTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl CodeContextTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl CodeImpactTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

fn supported_code_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "rs" | "py"
                | "js"
                | "jsx"
                | "ts"
                | "tsx"
                | "go"
                | "java"
                | "rb"
                | "php"
                | "swift"
                | "kt"
                | "kts"
                | "scala"
                | "c"
                | "cc"
                | "cpp"
                | "h"
                | "hpp"
        )
    )
}

fn kind_and_name_from_line(line: &str) -> Option<(&'static str, String)> {
    let trimmed = line.trim_start();
    let candidates = [
        ("fn", ["fn "].as_slice()),
        ("struct", ["struct ", "pub struct "].as_slice()),
        ("enum", ["enum ", "pub enum "].as_slice()),
        ("trait", ["trait ", "pub trait "].as_slice()),
        ("mod", ["mod ", "pub mod "].as_slice()),
        ("type", ["type ", "pub type "].as_slice()),
        ("class", ["class "].as_slice()),
        ("interface", ["interface "].as_slice()),
        ("def", ["def "].as_slice()),
    ];

    for (kind, prefixes) in candidates {
        for prefix in prefixes {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                    .collect();
                if !name.is_empty() {
                    return Some((kind, name));
                }
            }
        }
    }

    None
}

fn scan_symbol_matches(
    cwd: &Path,
    scope: &Path,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<SymbolMatch>> {
    let mut builder = WalkBuilder::new(scope);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_exclude(true);
    builder.ignore(true);

    let query = query.map(str::to_ascii_lowercase);
    let mut out = Vec::new();

    for dent in builder.build() {
        let dent = dent.map_err(|err| Error::tool("code.query", err.to_string()))?;
        if !dent.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = dent.path();
        if !supported_code_file(path) {
            continue;
        }
        let meta = fs::metadata(path).map_err(|err| {
            Error::tool(
                "code.query",
                format!("Failed to stat {}: {err}", path.display()),
            )
        })?;
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let content = fs::read_to_string(path).map_err(|err| {
            Error::tool(
                "code.query",
                format!("Failed to read {}: {err}", path.display()),
            )
        })?;
        for (idx, line) in content.lines().enumerate() {
            let Some((kind, name)) = kind_and_name_from_line(line) else {
                continue;
            };
            if let Some(query) = &query {
                let haystack = format!("{kind} {name} {}", line.trim()).to_ascii_lowercase();
                if !haystack.contains(query) {
                    continue;
                }
            }
            out.push(SymbolMatch {
                path: path.strip_prefix(cwd).unwrap_or(path).to_path_buf(),
                line: idx + 1,
                kind: kind.to_string(),
                name,
                line_text: line.trim().to_string(),
            });
            if out.len() >= limit {
                return Ok(out);
            }
        }
    }

    Ok(out)
}

fn format_symbol_matches(title: &str, matches: &[SymbolMatch]) -> String {
    if matches.is_empty() {
        return format!("{title}\n(no matches)");
    }

    let mut out = String::new();
    out.push_str(title);
    for item in matches {
        out.push_str("\n- ");
        out.push_str(&format!(
            "{}:{} [{}] {}",
            item.path.display(),
            item.line,
            item.kind,
            item.name
        ));
        if !item.line_text.is_empty() {
            out.push_str(" :: ");
            out.push_str(&item.line_text);
        }
    }
    out
}

fn extract_outline(path: &Path, limit: usize) -> Result<Vec<SymbolMatch>> {
    let content = fs::read_to_string(path).map_err(|err| {
        Error::tool(
            "code.context",
            format!("Failed to read {}: {err}", path.display()),
        )
    })?;
    let mut out = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let Some((kind, name)) = kind_and_name_from_line(line) else {
            continue;
        };
        out.push(SymbolMatch {
            path: path.to_path_buf(),
            line: idx + 1,
            kind: kind.to_string(),
            name,
            line_text: line.trim().to_string(),
        });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

fn excerpt_around(path: &Path, line_number: usize, before: usize, after: usize) -> Result<String> {
    let content = fs::read_to_string(path).map_err(|err| {
        Error::tool(
            "code.context",
            format!("Failed to read {}: {err}", path.display()),
        )
    })?;
    let lines: Vec<_> = content.lines().collect();
    if lines.is_empty() {
        return Ok(String::new());
    }
    let start = line_number.saturating_sub(before + 1);
    let end = (line_number + after).min(lines.len());
    let mut out = String::new();
    for (offset, line) in lines[start..end].iter().enumerate() {
        let actual = start + offset + 1;
        let marker = if actual == line_number { ">" } else { " " };
        out.push_str(&format!("{marker}{actual:>5} {line}\n"));
    }
    Ok(out.trim_end().to_string())
}

fn reference_lines(cwd: &Path, needle: &str, limit: usize) -> Result<Vec<String>> {
    let mut builder = WalkBuilder::new(cwd);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_exclude(true);
    builder.ignore(true);

    let mut out = Vec::new();
    for dent in builder.build() {
        let dent = dent.map_err(|err| Error::tool("code.impact", err.to_string()))?;
        if !dent.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = dent.path();
        if !supported_code_file(path) {
            continue;
        }
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        for (idx, line) in content.lines().enumerate() {
            if line.contains(needle) {
                out.push(format!(
                    "{}:{}: {}",
                    path.strip_prefix(cwd).unwrap_or(path).display(),
                    idx + 1,
                    line.trim()
                ));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

fn related_tests(cwd: &Path, needle: &str, limit: usize) -> Result<Vec<String>> {
    let mut builder = WalkBuilder::new(cwd);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_exclude(true);
    builder.ignore(true);

    let needle = needle.to_ascii_lowercase();
    let mut out = Vec::new();
    for dent in builder.build() {
        let dent = dent.map_err(|err| Error::tool("code.impact", err.to_string()))?;
        if !dent.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = dent.path();
        let display = path.strip_prefix(cwd).unwrap_or(path).display().to_string();
        let lower = display.to_ascii_lowercase();
        if (lower.contains("test") || lower.contains("spec")) && lower.contains(&needle) {
            out.push(display);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

#[async_trait]
impl Tool for CodeQueryTool {
    fn name(&self) -> &str {
        "code.query"
    }

    fn label(&self) -> &str {
        "code.query"
    }

    fn description(&self) -> &str {
        "Search code structure lexically across the repo and return compact symbol hits grouped for retrieval-first navigation."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search term, symbol fragment, or flow keyword." },
                "path": { "type": "string", "description": "Optional file or directory scope." },
                "limit": { "type": "integer", "description": "Maximum matches to return (default 20, max 200)." }
            }
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: CodeQueryInput =
            serde_json::from_value(input).map_err(|err| Error::validation(err.to_string()))?;
        let limit = input
            .limit
            .unwrap_or(DEFAULT_RESULT_LIMIT)
            .clamp(1, MAX_RESULT_LIMIT);
        let scope = input
            .path
            .as_deref()
            .map_or_else(|| self.cwd.clone(), |path| resolve_path(path, &self.cwd));
        let query = input
            .query
            .as_deref()
            .or(input.symbol.as_deref())
            .map(str::trim);
        let matches = scan_symbol_matches(&self.cwd, &scope, query, limit)?;
        let text = format_symbol_matches("code.query", &matches);

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({
                "scope": scope.display().to_string(),
                "query": query,
                "count": matches.len(),
            })),
            is_error: false,
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

#[async_trait]
impl Tool for CodeContextTool {
    fn name(&self) -> &str {
        "code.context"
    }

    fn label(&self) -> &str {
        "code.context"
    }

    fn description(&self) -> &str {
        "Return a focused code bundle: file outline first, then the most relevant symbol excerpt."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to outline or excerpt." },
                "symbol": { "type": "string", "description": "Optional symbol to resolve before excerpting." },
                "limit": { "type": "integer", "description": "Maximum outline items or symbol hits (default 20, max 200)." },
                "before": { "type": "integer", "description": "Context lines before the target line (default 8)." },
                "after": { "type": "integer", "description": "Context lines after the target line (default 20)." }
            }
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: CodeQueryInput =
            serde_json::from_value(input).map_err(|err| Error::validation(err.to_string()))?;
        let limit = input
            .limit
            .unwrap_or(DEFAULT_RESULT_LIMIT)
            .clamp(1, MAX_RESULT_LIMIT);
        let before = input.before.unwrap_or(8).clamp(0, 80);
        let after = input.after.unwrap_or(20).clamp(0, 120);
        let path = input
            .path
            .as_deref()
            .map_or_else(|| self.cwd.clone(), |path| resolve_path(path, &self.cwd));

        let mut sections = Vec::new();
        if path.is_file() {
            let outline = extract_outline(&path, limit)?;
            sections.push(format_symbol_matches("outline", &outline));
        }

        if let Some(symbol) = input
            .symbol
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let scope = if path.is_dir() {
                path.clone()
            } else {
                path.parent().unwrap_or(&self.cwd).to_path_buf()
            };
            let matches = scan_symbol_matches(&self.cwd, &scope, Some(symbol), limit)?;
            sections.push(format_symbol_matches("matches", &matches));
            if let Some(first) = matches.first() {
                let full_path = if first.path.is_absolute() {
                    first.path.clone()
                } else {
                    self.cwd.join(&first.path)
                };
                let excerpt = excerpt_around(&full_path, first.line, before, after)?;
                if !excerpt.is_empty() {
                    sections.push(format!("excerpt\n{}\n", excerpt));
                }
            }
        }

        if sections.is_empty() && path.is_file() {
            let excerpt = excerpt_around(&path, 1, 0, after)?;
            sections.push(format!("file\n{excerpt}"));
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(sections.join("\n\n")))],
            details: Some(json!({
                "path": path.display().to_string(),
                "symbol": input.symbol,
            })),
            is_error: false,
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

#[async_trait]
impl Tool for CodeImpactTool {
    fn name(&self) -> &str {
        "code.impact"
    }

    fn label(&self) -> &str {
        "code.impact"
    }

    fn description(&self) -> &str {
        "Estimate change impact by listing repo references and likely related tests for a symbol or path."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Primary symbol or identifier to analyze." },
                "path": { "type": "string", "description": "Optional file path whose stem should be analyzed." },
                "limit": { "type": "integer", "description": "Maximum reference/test lines to return (default 20, max 200)." }
            }
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: CodeQueryInput =
            serde_json::from_value(input).map_err(|err| Error::validation(err.to_string()))?;
        let limit = input
            .limit
            .unwrap_or(DEFAULT_RESULT_LIMIT)
            .clamp(1, MAX_RESULT_LIMIT);
        let needle = input
            .symbol
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                input.path.as_deref().and_then(|path| {
                    Path::new(path)
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(str::to_string)
                })
            })
            .ok_or_else(|| Error::validation("code.impact requires symbol or path".to_string()))?;

        let refs = reference_lines(&self.cwd, &needle, limit)?;
        let tests = related_tests(&self.cwd, &needle, limit.min(20))?;
        let mut text = String::from("code.impact");
        text.push_str("\nreferences");
        if refs.is_empty() {
            text.push_str("\n(no references)");
        } else {
            for item in &refs {
                text.push_str("\n- ");
                text.push_str(item);
            }
        }
        text.push_str("\n\ntests");
        if tests.is_empty() {
            text.push_str("\n(no related tests)");
        } else {
            for item in &tests {
                text.push_str("\n- ");
                text.push_str(item);
            }
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({
                "needle": needle,
                "referenceCount": refs.len(),
                "testCount": tests.len(),
            })),
            is_error: false,
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn scan_symbol_matches_finds_rust_definitions() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("lib.rs");
        fs::write(&file, "pub struct Alpha {}\nfn beta() {}\n").expect("write");

        let matches = scan_symbol_matches(dir.path(), dir.path(), Some("alpha"), 10).expect("scan");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, "struct");
        assert_eq!(matches[0].name, "Alpha");
    }

    #[test]
    fn excerpt_around_marks_target_line() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("mod.rs");
        fs::write(&file, "one\ntwo\nthree\nfour\n").expect("write");

        let excerpt = excerpt_around(&file, 3, 1, 1).expect("excerpt");

        assert!(excerpt.contains(">    3 three"));
        assert!(excerpt.contains("     2 two"));
        assert!(excerpt.contains("     4 four"));
    }
}
