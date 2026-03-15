use crate::context::compact_manifest::normalize_compact_value;
use crate::reliability::{ContextFanout, RetrievalPriority};
use std::path::{Path, PathBuf};

pub const DEFAULT_RETRIEVAL_MAX_TARGETS: usize = 4;
pub const DEFAULT_RETRIEVAL_MAX_RELATED_FILES: usize = 3;
pub const DEFAULT_RETRIEVAL_MAX_SYMBOL_LINES: usize = 6;
pub const DEFAULT_RETRIEVAL_BUDGET_TOKENS: usize = 900;

#[derive(Debug, Clone)]
pub struct RetrievalTarget {
    pub path: String,
    pub purpose: String,
}

#[derive(Debug, Clone)]
pub struct RetrievalSummaryRow {
    pub path: String,
    pub purpose: String,
    pub outline: String,
    pub related: String,
}

#[derive(Debug, Clone)]
pub struct RetrievalBundleOptions {
    pub max_targets: usize,
    pub max_related_per_target: usize,
    pub max_symbol_lines: usize,
    pub budget_tokens: usize,
}

impl Default for RetrievalBundleOptions {
    fn default() -> Self {
        Self {
            max_targets: DEFAULT_RETRIEVAL_MAX_TARGETS,
            max_related_per_target: DEFAULT_RETRIEVAL_MAX_RELATED_FILES,
            max_symbol_lines: DEFAULT_RETRIEVAL_MAX_SYMBOL_LINES,
            budget_tokens: DEFAULT_RETRIEVAL_BUDGET_TOKENS,
        }
    }
}

pub fn collect_retrieval_summary_rows(
    workspace_root: &Path,
    targets: &[RetrievalTarget],
    options: &RetrievalBundleOptions,
) -> Vec<RetrievalSummaryRow> {
    targets
        .iter()
        .take(options.max_targets)
        .map(|target| summarize_target(workspace_root, target, options))
        .collect()
}

fn summarize_target(
    workspace_root: &Path,
    target: &RetrievalTarget,
    options: &RetrievalBundleOptions,
) -> RetrievalSummaryRow {
    let resolved = resolve_target_path(workspace_root, &target.path);
    let path = relative_path_label(workspace_root, &resolved);
    let purpose = normalize_compact_value(&target.purpose, 72);

    if resolved.is_dir() {
        return RetrievalSummaryRow {
            path,
            purpose,
            outline: "directory scope".to_string(),
            related: directory_entry_hint(
                &resolved,
                workspace_root,
                options.max_related_per_target,
            ),
        };
    }

    if !resolved.exists() {
        return RetrievalSummaryRow {
            path,
            purpose,
            outline: "planned new file".to_string(),
            related: nearby_file_hint(workspace_root, &resolved, options.max_related_per_target),
        };
    }

    let outline = symbol_outline_for_file(&resolved, options.max_symbol_lines);
    let related = ContextFanout::gather(&resolved, workspace_root, options.budget_tokens)
        .map(|fanout| related_summary(&fanout, workspace_root, &resolved, options))
        .unwrap_or_else(|_| "fanout unavailable".to_string());

    RetrievalSummaryRow {
        path,
        purpose,
        outline,
        related,
    }
}

fn resolve_target_path(workspace_root: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root.join(candidate)
    }
}

pub fn relative_path_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub fn symbol_outline_for_file(path: &Path, max_symbol_lines: usize) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return "unavailable".to_string();
    };

    let mut symbols = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        let is_symbol = trimmed.starts_with("pub struct ")
            || trimmed.starts_with("struct ")
            || trimmed.starts_with("pub enum ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("pub trait ")
            || trimmed.starts_with("trait ")
            || trimmed.starts_with("impl ")
            || trimmed.starts_with("pub fn ")
            || trimmed.starts_with("fn ")
            || trimmed.starts_with("pub async fn ")
            || trimmed.starts_with("async fn ")
            || trimmed.starts_with("export function ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("export class ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("interface ")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("def ");

        if is_symbol {
            symbols.push(trim_symbol_line(trimmed));
        }
        if symbols.len() >= max_symbol_lines {
            break;
        }
    }

    if symbols.is_empty() {
        content
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(trim_symbol_line)
            .unwrap_or_else(|| "no obvious top-level symbols".to_string())
    } else {
        symbols.join("; ")
    }
}

fn trim_symbol_line(line: &str) -> String {
    normalize_compact_value(line, 88)
}

fn related_summary(
    fanout: &ContextFanout,
    workspace_root: &Path,
    target_path: &Path,
    options: &RetrievalBundleOptions,
) -> String {
    let mut rendered = Vec::new();
    let mut remaining = options.max_related_per_target;

    for (priority, paths) in &fanout.files_by_priority {
        if remaining == 0 {
            break;
        }

        let mut current = Vec::new();
        for path in paths {
            if remaining == 0 {
                break;
            }

            let related_path = Path::new(path);
            if related_path == target_path || !related_path.is_file() {
                continue;
            }
            current.push(relative_path_label(workspace_root, related_path));
            remaining -= 1;
        }

        if !current.is_empty() {
            rendered.push(format!(
                "{}:{}",
                retrieval_priority_label(*priority),
                current.join(",")
            ));
        }
    }

    if rendered.is_empty() {
        "none".to_string()
    } else {
        normalize_compact_value(&rendered.join("; "), 120)
    }
}

fn retrieval_priority_label(priority: RetrievalPriority) -> &'static str {
    match priority {
        RetrievalPriority::DirectImports => "imports",
        RetrievalPriority::TypeDefinitions => "types",
        RetrievalPriority::TestFiles => "tests",
        RetrievalPriority::SimilarPatterns => "similar",
    }
}

fn nearby_file_hint(workspace_root: &Path, path: &Path, max_entries: usize) -> String {
    let Some(parent) = path.parent() else {
        return "none".to_string();
    };
    directory_entry_hint(parent, workspace_root, max_entries)
}

fn directory_entry_hint(directory: &Path, workspace_root: &Path, max_entries: usize) -> String {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return "none".to_string();
    };
    let mut paths = entries
        .flatten()
        .map(|entry| relative_path_label(workspace_root, &entry.path()))
        .collect::<Vec<_>>();
    paths.sort();
    paths.truncate(max_entries);

    if paths.is_empty() {
        "none".to_string()
    } else {
        normalize_compact_value(&paths.join(", "), 120)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn collect_retrieval_summary_rows_renders_outline_and_related_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        fs::create_dir_all(root.join("src")).expect("mkdir src");
        fs::write(
            root.join("src/lib.rs"),
            "use crate::helper::Thing;\n\npub struct Demo;\n\npub fn run() {}\n",
        )
        .expect("write lib");
        fs::write(root.join("src/helper.rs"), "pub struct Thing;\n").expect("write helper");

        let rows = collect_retrieval_summary_rows(
            root,
            &[RetrievalTarget {
                path: "src/lib.rs".to_string(),
                purpose: "readx2 mutatex1".to_string(),
            }],
            &RetrievalBundleOptions::default(),
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "src/lib.rs");
        assert!(rows[0].outline.contains("pub struct Demo"));
    }
}
