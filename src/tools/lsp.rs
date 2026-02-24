use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};

use super::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Tool, ToolOutput, ToolUpdate, resolve_path, rg_available,
    truncate_tail,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LspInput {
    action: Option<String>,
    symbol: Option<String>,
    path: Option<String>,
    limit: Option<usize>,
}

pub struct LspTool {
    cwd: PathBuf,
}

impl LspTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

fn run_repo_search(cwd: &Path, scope: &Path, pattern: &str, limit: usize) -> Result<Vec<String>> {
    if !rg_available() {
        return Err(Error::tool(
            "lsp",
            "ripgrep (`rg`) is required for lsp symbol search".to_string(),
        ));
    }

    let limit_str = limit.to_string();
    let scope_str = scope.display().to_string();
    let output = Command::new("rg")
        .args([
            "--line-number",
            "--no-heading",
            "--color",
            "never",
            "--max-count",
            &limit_str,
            "--",
            pattern,
            &scope_str,
        ])
        .current_dir(cwd)
        .output()
        .map_err(|e| Error::tool("lsp", format!("Failed to run rg: {e}")))?;

    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::tool("lsp", format!("rg failed: {}", stderr.trim())));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(str::to_string).collect())
}

fn extract_symbols_from_file(path: &Path, limit: usize) -> Result<Vec<String>> {
    static SYMBOL_RE: OnceLock<Regex> = OnceLock::new();
    let re = SYMBOL_RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:pub\s+)?(?:async\s+)?(?:fn|struct|enum|trait|type|mod|impl|class|interface|def)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("valid symbol regex")
    });

    let bytes =
        std::fs::read(path).map_err(|e| Error::tool("lsp", format!("Failed to read file: {e}")))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();

    for (idx, line) in text.lines().enumerate() {
        if let Some(caps) = re.captures(line)
            && let Some(name) = caps.get(1)
        {
            out.push(format!("{}:{}: {}", path.display(), idx + 1, name.as_str()));
            if out.len() >= limit {
                break;
            }
        }
    }

    Ok(out)
}

fn run_rust_diagnostics(cwd: &Path, path_filter: Option<&Path>) -> Result<(String, bool)> {
    let output = Command::new("cargo")
        .args(["check", "--all-targets", "--message-format", "short"])
        .current_dir(cwd)
        .output()
        .map_err(|e| Error::tool("lsp", format!("Failed to run cargo check: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut lines: Vec<String> = format!("{stdout}\n{stderr}")
        .lines()
        .map(str::to_string)
        .collect();

    if let Some(path) = path_filter {
        let needle = path.display().to_string();
        lines.retain(|line| line.contains(&needle));
    }

    if lines.is_empty() && output.status.success() {
        return Ok(("No diagnostics reported by cargo check.".to_string(), false));
    }

    if lines.is_empty() {
        lines = vec!["cargo check failed, but no filtered diagnostics were found.".to_string()];
    }

    let truncation = truncate_tail(lines.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut text = truncation.content;
    if truncation.truncated {
        text.push_str("\n\n[Truncated diagnostics output]");
    }

    Ok((text, !output.status.success()))
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &'static str {
        "lsp"
    }

    fn label(&self) -> &'static str {
        "lsp"
    }

    fn description(&self) -> &'static str {
        "Lightweight LSP-style tool for symbol definitions/references, file symbol listing, and Rust diagnostics via cargo check."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "One of: definition, references, symbols, diagnostics"
                },
                "symbol": {
                    "type": "string",
                    "description": "Symbol name for definition/references lookups"
                },
                "path": {
                    "type": "string",
                    "description": "Optional file or directory scope (required for file symbol listing)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum result count (default 20, max 200)"
                }
            }
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: LspInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let action = input
            .action
            .as_deref()
            .unwrap_or("definition")
            .trim()
            .to_ascii_lowercase();
        let limit = input.limit.unwrap_or(20).clamp(1, 200);

        if action == "diagnostics" {
            let path_filter = input.path.as_deref().map(|p| resolve_path(p, &self.cwd));
            let rust_mode = path_filter
                .as_ref()
                .and_then(|p| p.extension())
                .is_none_or(|ext| ext == "rs")
                && self.cwd.join("Cargo.toml").exists();

            let (text, is_error) = if rust_mode {
                run_rust_diagnostics(&self.cwd, path_filter.as_deref())?
            } else {
                (
                    "No built-in diagnostics backend for this file type yet. Use the `bash` tool with your language checker."
                        .to_string(),
                    false,
                )
            };

            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                details: Some(serde_json::json!({
                    "action": action,
                    "path": input.path,
                    "rustMode": rust_mode,
                })),
                is_error,
            });
        }

        let results = match action.as_str() {
            "symbols" => {
                if let Some(path) = input.path.as_deref() {
                    let resolved = resolve_path(path, &self.cwd);
                    if resolved.is_file() {
                        extract_symbols_from_file(&resolved, limit)?
                    } else {
                        let pattern = r"\b(fn|struct|enum|trait|type|mod|class|interface|def)\s+[A-Za-z_][A-Za-z0-9_]*\b";
                        run_repo_search(&self.cwd, &resolved, pattern, limit)?
                    }
                } else {
                    let pattern = r"\b(fn|struct|enum|trait|type|mod|class|interface|def)\s+[A-Za-z_][A-Za-z0-9_]*\b";
                    run_repo_search(&self.cwd, &self.cwd, pattern, limit)?
                }
            }
            "definition" | "references" => {
                let symbol = input.symbol.as_deref().unwrap_or("").trim();
                if symbol.is_empty() {
                    return Err(Error::validation(format!(
                        "lsp.symbol is required for action={action}"
                    )));
                }
                let escaped = regex::escape(symbol);
                let pattern = if action == "definition" {
                    format!(
                        r"\b(fn|struct|enum|trait|type|mod|class|interface|def|const|let|var)\s+{escaped}\b"
                    )
                } else {
                    format!(r"\b{escaped}\b")
                };

                let scope = input
                    .path
                    .as_deref()
                    .map_or_else(|| self.cwd.clone(), |p| resolve_path(p, &self.cwd));
                run_repo_search(&self.cwd, &scope, &pattern, limit)?
            }
            _ => {
                return Err(Error::validation(
                    "lsp.action must be one of: definition, references, symbols, diagnostics"
                        .to_string(),
                ));
            }
        };

        let text = if results.is_empty() {
            "No matches found.".to_string()
        } else {
            results.join("\n")
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(serde_json::json!({
                "action": action,
                "count": results.len(),
                "path": input.path,
                "symbol": input.symbol,
            })),
            is_error: false,
        })
    }
}
