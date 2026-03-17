use ignore::WalkBuilder;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::lsp::registry::rust_backend_available;

const MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedSymbol {
    pub path: PathBuf,
    pub line: usize,
    pub kind: String,
    pub name: String,
    pub line_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub path: PathBuf,
    pub imports: Vec<String>,
    pub symbols: Vec<IndexedSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectiveRetrievalBundle {
    pub objective: String,
    pub semantic_backend: Option<String>,
    pub matched_symbols: Vec<IndexedSymbol>,
    pub related_imports: Vec<String>,
    pub related_tests: Vec<String>,
}

impl ObjectiveRetrievalBundle {
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("objective=");
        out.push_str(&self.objective);
        out.push_str(
            "\nretrieval_ladder=code.query -> code.context -> code.impact -> read -> grep/find",
        );
        out.push_str("\nbundle_contract=compact_code_context_bundle_with_semantic_hints");

        if let Some(semantic_backend) = &self.semantic_backend {
            out.push_str("\nsemantic_backend=");
            out.push_str(semantic_backend);
        }

        out.push_str("\n\nsymbols");
        if self.matched_symbols.is_empty() {
            out.push_str("\n- (no ranked symbols)");
        } else {
            for symbol in &self.matched_symbols {
                out.push_str("\n- ");
                out.push_str(&format!(
                    "{}:{} [{}] {}",
                    symbol.path.display(),
                    symbol.line,
                    symbol.kind,
                    symbol.name
                ));
                if !symbol.line_text.is_empty() {
                    out.push_str(" :: ");
                    out.push_str(&symbol.line_text);
                }
            }
        }

        out.push_str("\n\nimports");
        if self.related_imports.is_empty() {
            out.push_str("\n- (no ranked imports)");
        } else {
            for import in &self.related_imports {
                out.push_str("\n- ");
                out.push_str(import);
            }
        }

        out.push_str("\n\ntests");
        if self.related_tests.is_empty() {
            out.push_str("\n- (no related tests)");
        } else {
            for test in &self.related_tests {
                out.push_str("\n- ");
                out.push_str(test);
            }
        }

        out
    }
}

#[derive(Debug, Clone)]
pub struct RepoCodeIndex {
    root: PathBuf,
    files: Vec<IndexedFile>,
}

impl RepoCodeIndex {
    pub fn build(root: &Path, scope: &Path) -> Result<Self> {
        let mut builder = WalkBuilder::new(scope);
        builder.hidden(false);
        builder.git_ignore(true);
        builder.git_exclude(true);
        builder.ignore(true);

        let mut files = Vec::new();
        for dent in builder.build() {
            let dent = dent.map_err(|err| Error::tool("code.index", err.to_string()))?;
            if !dent.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = dent.path();
            if !supported_code_file(path) {
                continue;
            }
            let metadata = fs::metadata(path).map_err(|err| {
                Error::tool(
                    "code.index",
                    format!("Failed to stat {}: {err}", path.display()),
                )
            })?;
            if metadata.len() > MAX_FILE_BYTES {
                continue;
            }
            let content = fs::read_to_string(path).map_err(|err| {
                Error::tool(
                    "code.index",
                    format!("Failed to read {}: {err}", path.display()),
                )
            })?;
            let relative = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            files.push(IndexedFile {
                path: relative.clone(),
                imports: extract_imports(&content, &relative),
                symbols: extract_symbols(&content, &relative),
            });
        }

        Ok(Self {
            root: root.to_path_buf(),
            files,
        })
    }

    #[must_use]
    pub fn query_symbols(&self, query: Option<&str>, limit: usize) -> Vec<IndexedSymbol> {
        let query = query.map(str::trim).filter(|value| !value.is_empty());
        let query = query.map(str::to_ascii_lowercase);
        let mut out = Vec::new();

        for file in &self.files {
            for symbol in &file.symbols {
                if let Some(query) = &query {
                    let haystack = format!(
                        "{} {} {}",
                        symbol.kind,
                        symbol.name,
                        symbol.line_text.to_ascii_lowercase()
                    )
                    .to_ascii_lowercase();
                    if !haystack.contains(query) {
                        continue;
                    }
                }
                out.push(symbol.clone());
            }
        }

        out.sort_by_key(|symbol| {
            (
                is_test_path(&symbol.path),
                symbol.path.components().count(),
                symbol.line,
            )
        });
        out.truncate(limit);
        out
    }

    #[must_use]
    pub fn outline(&self, path: &Path, limit: usize) -> Vec<IndexedSymbol> {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        self.files
            .iter()
            .find(|file| file.path == relative)
            .map(|file| file.symbols.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn imports_for(&self, path: &Path, limit: usize) -> Vec<String> {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        self.files
            .iter()
            .find(|file| file.path == relative)
            .map(|file| file.imports.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn reference_lines(&self, needle: &str, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        for file in &self.files {
            let full_path = self.root.join(&file.path);
            let content = match fs::read_to_string(&full_path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            for (idx, line) in content.lines().enumerate() {
                if line.contains(needle) {
                    out.push(format!(
                        "{}:{}: {}",
                        file.path.display(),
                        idx + 1,
                        line.trim()
                    ));
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    #[must_use]
    pub fn related_tests(&self, needle: &str, limit: usize) -> Vec<String> {
        let needle = needle.to_ascii_lowercase();
        let mut out = Vec::new();
        for file in &self.files {
            let display = file.path.display().to_string();
            let lower = display.to_ascii_lowercase();
            if (lower.contains("test") || lower.contains("spec")) && lower.contains(&needle) {
                out.push(display);
                if out.len() >= limit {
                    return out;
                }
            }
        }
        out
    }

    #[must_use]
    pub fn objective_bundle(&self, objective: &str, limit: usize) -> ObjectiveRetrievalBundle {
        let terms = objective_terms(objective);
        let mut symbols = Vec::new();
        let mut imports = BTreeSet::new();
        let mut tests = BTreeSet::new();

        for term in &terms {
            for symbol in self.query_symbols(Some(term), limit) {
                if symbols.iter().any(|existing: &IndexedSymbol| {
                    existing.path == symbol.path && existing.line == symbol.line
                }) {
                    continue;
                }
                let full_path = self.root.join(&symbol.path);
                for import in self.imports_for(&full_path, 6) {
                    imports.insert(import);
                    if imports.len() >= limit {
                        break;
                    }
                }
                for test in self.related_tests(&symbol.name, limit.min(12)) {
                    tests.insert(test);
                    if tests.len() >= limit.min(12) {
                        break;
                    }
                }
                symbols.push(symbol);
                if symbols.len() >= limit {
                    break;
                }
            }
            if symbols.len() >= limit {
                break;
            }
        }

        if imports.is_empty() {
            for symbol in &symbols {
                let full_path = self.root.join(&symbol.path);
                for import in self.imports_for(&full_path, 6) {
                    imports.insert(import);
                    if imports.len() >= limit {
                        break;
                    }
                }
                if imports.len() >= limit {
                    break;
                }
            }
        }

        if imports.is_empty() {
            for file in &self.files {
                let path_lower = file.path.display().to_string().to_ascii_lowercase();
                let matches_objective = terms.iter().any(|term| {
                    let term_lower = term.to_ascii_lowercase();
                    path_lower.contains(&term_lower)
                        || file
                            .symbols
                            .iter()
                            .any(|symbol| symbol.name.to_ascii_lowercase().contains(&term_lower))
                });
                if !matches_objective || file.imports.is_empty() {
                    continue;
                }
                for import in &file.imports {
                    imports.insert(import.clone());
                    if imports.len() >= limit {
                        break;
                    }
                }
                if imports.len() >= limit {
                    break;
                }
            }
        }

        ObjectiveRetrievalBundle {
            objective: objective.trim().to_string(),
            semantic_backend: rust_backend_available(&self.root)
                .then_some("rust-diagnostics".to_string()),
            matched_symbols: symbols,
            related_imports: imports.into_iter().take(limit).collect(),
            related_tests: tests.into_iter().take(limit.min(12)).collect(),
        }
    }
}

pub fn build_objective_retrieval_bundle(
    root: &Path,
    objective: &str,
    limit: usize,
) -> Result<ObjectiveRetrievalBundle> {
    let index = RepoCodeIndex::build(root, root)?;
    Ok(index.objective_bundle(objective, limit))
}

pub fn supported_code_file(path: &Path) -> bool {
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

pub fn kind_and_name_from_line(line: &str) -> Option<(&'static str, String)> {
    let trimmed = line.trim_start();
    let candidates = [
        (
            "fn",
            [
                "fn ",
                "pub fn ",
                "async fn ",
                "pub async fn ",
                "pub(crate) fn ",
                "pub(crate) async fn ",
                "pub(super) fn ",
                "pub(super) async fn ",
            ]
            .as_slice(),
        ),
        (
            "struct",
            [
                "struct ",
                "pub struct ",
                "pub(crate) struct ",
                "pub(super) struct ",
            ]
            .as_slice(),
        ),
        (
            "enum",
            ["enum ", "pub enum ", "pub(crate) enum ", "pub(super) enum "].as_slice(),
        ),
        (
            "trait",
            [
                "trait ",
                "pub trait ",
                "pub(crate) trait ",
                "pub(super) trait ",
            ]
            .as_slice(),
        ),
        (
            "mod",
            ["mod ", "pub mod ", "pub(crate) mod ", "pub(super) mod "].as_slice(),
        ),
        (
            "type",
            ["type ", "pub type ", "pub(crate) type ", "pub(super) type "].as_slice(),
        ),
        ("class", ["class "].as_slice()),
        ("interface", ["interface "].as_slice()),
        ("def", ["def ", "async def "].as_slice()),
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

fn is_test_path(path: &Path) -> bool {
    let lower = path.display().to_string().to_ascii_lowercase();
    lower.contains("test") || lower.contains("spec")
}

fn extract_symbols(content: &str, relative_path: &Path) -> Vec<IndexedSymbol> {
    let mut out = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let Some((kind, name)) = kind_and_name_from_line(line) else {
            continue;
        };
        out.push(IndexedSymbol {
            path: relative_path.to_path_buf(),
            line: idx + 1,
            kind: kind.to_string(),
            name,
            line_text: line.trim().to_string(),
        });
    }
    out
}

fn extract_imports(content: &str, relative_path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let is_import = trimmed.starts_with("use ")
            || trimmed.starts_with("pub use ")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed == "import"
            || trimmed.starts_with("#include ")
            || trimmed.starts_with("require(");
        if is_import {
            out.push(format!(
                "{}:{}: {}",
                relative_path.display(),
                idx + 1,
                trimmed
            ));
        }
    }
    out
}

fn objective_terms(objective: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "the", "and", "for", "with", "from", "into", "that", "this", "then", "when", "have",
        "need", "make", "does", "code", "tool", "turn", "task", "agent", "fix",
    ];

    objective
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == ':'))
        .map(str::trim)
        .filter(|term| term.len() >= 3)
        .filter(|term| !STOP_WORDS.contains(&term.to_ascii_lowercase().as_str()))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn repo_code_index_collects_symbols_and_imports() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("lib.rs"),
            "use crate::db::Store;\npub struct Alpha {}\nfn beta() {}\n",
        )
        .expect("write");

        let index = RepoCodeIndex::build(dir.path(), dir.path()).expect("index");
        let symbols = index.query_symbols(Some("alpha"), 10);
        let imports = index.imports_for(&dir.path().join("lib.rs"), 10);

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Alpha");
        assert_eq!(imports.len(), 1);
        assert!(imports[0].contains("use crate::db::Store;"));
    }

    #[test]
    fn repo_code_index_related_tests_match_symbol_name() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("alpha.rs"), "pub fn alpha_service() {}\n").expect("write");
        fs::create_dir_all(dir.path().join("tests")).expect("mkdir");
        fs::write(
            dir.path().join("tests").join("alpha_service_spec.rs"),
            "#[test]\nfn alpha_service_spec() {}\n",
        )
        .expect("write test");

        let index = RepoCodeIndex::build(dir.path(), dir.path()).expect("index");
        let tests = index.related_tests("alpha_service", 10);

        assert_eq!(tests, vec!["tests/alpha_service_spec.rs".to_string()]);
    }

    #[test]
    fn objective_bundle_ranks_symbols_imports_and_tests() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("cache.rs"),
            "use crate::db::Store;\npub fn cache_invalidator() {}\n",
        )
        .expect("write cache");
        fs::create_dir_all(dir.path().join("tests")).expect("mkdir");
        fs::write(
            dir.path().join("tests").join("cache_invalidator_test.rs"),
            "#[test]\nfn cache_invalidator_test() {}\n",
        )
        .expect("write test");

        let bundle = build_objective_retrieval_bundle(dir.path(), "fix cache invalidator", 8)
            .expect("bundle");

        assert!(!bundle.matched_symbols.is_empty());
        assert!(
            bundle
                .related_imports
                .iter()
                .any(|line| line.contains("Store"))
        );
        assert!(
            bundle
                .related_tests
                .iter()
                .any(|path| path.contains("cache_invalidator_test"))
        );
        assert!(bundle.render().contains("bundle_contract="));
    }

    #[test]
    fn query_symbols_prefers_implementation_files_over_tests() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("service.rs"),
            "pub fn retry_scheduler() {}\n",
        )
        .expect("write");
        fs::create_dir_all(dir.path().join("tests")).expect("mkdir");
        fs::write(
            dir.path().join("tests").join("retry_scheduler_test.rs"),
            "#[test]\nfn retry_scheduler_test() {}\n",
        )
        .expect("write test");

        let index = RepoCodeIndex::build(dir.path(), dir.path()).expect("index");
        let symbols = index.query_symbols(Some("retry_scheduler"), 4);

        assert!(!symbols.is_empty());
        assert_eq!(symbols[0].path, PathBuf::from("service.rs"));
    }
}
