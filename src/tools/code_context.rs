use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::lsp::registry::rust_backend_available;
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

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum CodeBundleKind {
    Query,
    Context,
    Impact,
}

impl CodeBundleKind {
    const fn tool_name(self) -> &'static str {
        match self {
            Self::Query => "code.query",
            Self::Context => "code.context",
            Self::Impact => "code.impact",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodeBundleItem {
    path: String,
    line: usize,
    kind: String,
    name: String,
    summary: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodeBundleExcerpt {
    path: String,
    anchor_line: usize,
    snippet: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodeContextBundle {
    kind: CodeBundleKind,
    scope: String,
    focus: Option<String>,
    semantic_backend: Option<String>,
    overview: Vec<String>,
    symbols: Vec<CodeBundleItem>,
    excerpts: Vec<CodeBundleExcerpt>,
    references: Vec<String>,
    related_tests: Vec<String>,
}

impl CodeContextBundle {
    fn new(kind: CodeBundleKind, scope: String, focus: Option<String>, cwd: &Path) -> Self {
        let semantic_backend =
            rust_backend_available(cwd).then_some("rust-diagnostics".to_string());
        Self {
            kind,
            scope,
            focus,
            semantic_backend,
            overview: Vec::new(),
            symbols: Vec::new(),
            excerpts: Vec::new(),
            references: Vec::new(),
            related_tests: Vec::new(),
        }
    }

    fn push_overview(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !line.trim().is_empty() {
            self.overview.push(line);
        }
    }

    fn push_symbols(&mut self, matches: &[SymbolMatch]) {
        self.symbols
            .extend(matches.iter().map(|item| CodeBundleItem {
                path: item.path.display().to_string(),
                line: item.line,
                kind: item.kind.clone(),
                name: item.name.clone(),
                summary: item.line_text.clone(),
            }));
    }

    fn push_excerpt(&mut self, path: &Path, anchor_line: usize, snippet: String) {
        if snippet.trim().is_empty() {
            return;
        }
        self.excerpts.push(CodeBundleExcerpt {
            path: path.display().to_string(),
            anchor_line,
            snippet,
        });
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(self.kind.tool_name());
        out.push_str("\nscope=");
        out.push_str(&self.scope);

        if let Some(focus) = &self.focus {
            out.push_str("\nfocus=");
            out.push_str(focus);
        }
        if let Some(semantic_backend) = &self.semantic_backend {
            out.push_str("\nsemantic_backend=");
            out.push_str(semantic_backend);
        }

        if !self.overview.is_empty() {
            out.push_str("\n\noverview");
            for line in &self.overview {
                out.push_str("\n- ");
                out.push_str(line);
            }
        }

        if !self.symbols.is_empty() {
            out.push_str("\n\nsymbols");
            for item in &self.symbols {
                out.push_str("\n- ");
                out.push_str(&format!(
                    "{}:{} [{}] {}",
                    item.path, item.line, item.kind, item.name
                ));
                if !item.summary.trim().is_empty() {
                    out.push_str(" :: ");
                    out.push_str(&item.summary);
                }
            }
        }

        if !self.excerpts.is_empty() {
            out.push_str("\n\ncontext");
            for excerpt in &self.excerpts {
                out.push_str("\n- ");
                out.push_str(&format!(
                    "{}:{}\n{}",
                    excerpt.path, excerpt.anchor_line, excerpt.snippet
                ));
            }
        }

        if !self.references.is_empty() {
            out.push_str("\n\nreferences");
            for item in &self.references {
                out.push_str("\n- ");
                out.push_str(item);
            }
        }

        if !self.related_tests.is_empty() {
            out.push_str("\n\ntests");
            for item in &self.related_tests {
                out.push_str("\n- ");
                out.push_str(item);
            }
        }

        out
    }

    fn details(&self) -> serde_json::Value {
        json!(self)
    }
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
        let mut bundle = CodeContextBundle::new(
            CodeBundleKind::Query,
            scope.display().to_string(),
            query.map(str::to_string),
            &self.cwd,
        );
        bundle.push_overview(format!("symbol_hits={}", matches.len()));
        bundle.push_overview("retrieval_next=code.context");
        if matches.is_empty() {
            bundle.push_overview("status=no_symbol_matches");
        }
        bundle.push_symbols(&matches);

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(bundle.render()))],
            details: Some(bundle.details()),
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

        let focus = input
            .symbol
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            });
        let mut bundle = CodeContextBundle::new(
            CodeBundleKind::Context,
            path.display().to_string(),
            focus,
            &self.cwd,
        );

        if path.is_file() {
            let outline = extract_outline(&path, limit)?;
            bundle.push_overview(format!("outline_items={}", outline.len()));
            bundle.push_symbols(&outline);
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
            bundle.push_overview(format!("symbol_hits={}", matches.len()));
            bundle.push_symbols(&matches);
            if let Some(first) = matches.first() {
                let full_path = if first.path.is_absolute() {
                    first.path.clone()
                } else {
                    self.cwd.join(&first.path)
                };
                let excerpt = excerpt_around(&full_path, first.line, before, after)?;
                bundle.push_excerpt(&full_path, first.line, excerpt);
            }
        }

        if bundle.symbols.is_empty() && path.is_file() {
            let excerpt = excerpt_around(&path, 1, 0, after)?;
            bundle.push_overview("fallback=file_excerpt");
            bundle.push_excerpt(&path, 1, excerpt);
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(bundle.render()))],
            details: Some(bundle.details()),
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
        let mut bundle = CodeContextBundle::new(
            CodeBundleKind::Impact,
            self.cwd.display().to_string(),
            Some(needle.clone()),
            &self.cwd,
        );
        bundle.push_overview(format!("reference_hits={}", refs.len()));
        bundle.push_overview(format!("related_tests={}", tests.len()));
        if refs.is_empty() {
            bundle.push_overview("status=no_repo_references");
        }
        bundle.references = refs;
        bundle.related_tests = tests;

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(bundle.render()))],
            details: Some(bundle.details()),
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

    #[test]
    fn code_context_bundle_renders_compact_sections() {
        let mut bundle = CodeContextBundle::new(
            CodeBundleKind::Context,
            "src/lib.rs".to_string(),
            Some("Alpha".to_string()),
            Path::new("."),
        );
        bundle.push_overview("outline_items=1");
        bundle.push_symbols(&[SymbolMatch {
            path: PathBuf::from("src/lib.rs"),
            line: 4,
            kind: "struct".to_string(),
            name: "Alpha".to_string(),
            line_text: "pub struct Alpha {}".to_string(),
        }]);
        bundle.push_excerpt(
            Path::new("src/lib.rs"),
            4,
            ">    4 pub struct Alpha {}".to_string(),
        );
        bundle
            .references
            .push("src/main.rs:9: Alpha::new()".to_string());

        let rendered = bundle.render();
        assert!(rendered.contains("code.context"));
        assert!(rendered.contains("scope=src/lib.rs"));
        assert!(rendered.contains("symbols"));
        assert!(rendered.contains("context"));
        assert!(rendered.contains("references"));
    }
}
