use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSection {
    pub name: String,
    pub priority: u8,
    pub min_tokens: usize,
    pub max_tokens: usize,
    pub content: String,
}

impl ContextSection {
    pub fn estimate_tokens(&self) -> usize {
        estimate_tokens(&self.content)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetedContext {
    pub total_budget: usize,
    pub total_used_tokens: usize,
    pub rendered: String,
    #[serde(default)]
    pub included_sections: Vec<String>,
    #[serde(default)]
    pub truncated_sections: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalContextInput {
    pub critical_rules: String,
    pub active_task_contract: String,
    pub latest_evidence: String,
    pub compacted_state_digest: String,
    pub code_retrieval: String,
}

pub trait RetrievalAdapter {
    fn retrieve(&self, local: &RetrievalContextInput) -> Result<RetrievalContextInput, String>;
}

pub struct ContextBudgetComposer;

impl ContextBudgetComposer {
    pub fn compose(total_budget: usize, mut sections: Vec<ContextSection>) -> BudgetedContext {
        sections.sort_by_key(|section| std::cmp::Reverse(section.priority));

        let mut remaining = total_budget;
        let mut rendered = Vec::new();
        let mut included = Vec::new();
        let mut truncated = Vec::new();

        for section in sections {
            if remaining == 0 {
                truncated.push(section.name);
                continue;
            }

            let available_cap = section.max_tokens.min(remaining);
            let min_required = section.min_tokens.min(available_cap);
            if min_required == 0 {
                truncated.push(section.name);
                continue;
            }

            let clipped = clip_text_to_tokens(&section.content, available_cap);
            let used = estimate_tokens(&clipped);
            if used < min_required {
                truncated.push(section.name);
                continue;
            }

            remaining = remaining.saturating_sub(used);
            included.push(section.name.clone());
            if used < section.estimate_tokens() {
                truncated.push(section.name.clone());
            }
            rendered.push(format!("## {}\n{}", section.name, clipped));
        }

        BudgetedContext {
            total_budget,
            total_used_tokens: total_budget.saturating_sub(remaining),
            rendered: rendered.join("\n\n"),
            included_sections: included,
            truncated_sections: truncated,
        }
    }

    pub fn compose_retrieval_first(
        total_budget: usize,
        input: RetrievalContextInput,
    ) -> BudgetedContext {
        Self::compose_retrieval_first_with_adapter(total_budget, input, None)
    }

    pub fn compose_retrieval_first_with_adapter(
        total_budget: usize,
        input: RetrievalContextInput,
        adapter: Option<&dyn RetrievalAdapter>,
    ) -> BudgetedContext {
        let selected_input = match adapter {
            Some(adapter) => adapter.retrieve(&input).unwrap_or(input),
            None => input,
        };
        let sections = vec![
            ContextSection {
                name: "critical_rules".to_string(),
                priority: 100,
                min_tokens: 1,
                max_tokens: (total_budget / 5).max(1),
                content: selected_input.critical_rules,
            },
            ContextSection {
                name: "active_task_contract".to_string(),
                priority: 90,
                min_tokens: 1,
                max_tokens: (total_budget * 3 / 10).max(1),
                content: selected_input.active_task_contract,
            },
            ContextSection {
                name: "latest_evidence".to_string(),
                priority: 80,
                min_tokens: 1,
                max_tokens: (total_budget / 4).max(1),
                content: selected_input.latest_evidence,
            },
            ContextSection {
                name: "compacted_state_digest".to_string(),
                priority: 70,
                min_tokens: 1,
                max_tokens: (total_budget / 5).max(1),
                content: selected_input.compacted_state_digest,
            },
            ContextSection {
                name: "code_retrieval".to_string(),
                priority: 60,
                min_tokens: 1,
                max_tokens: (total_budget * 2 / 5).max(1),
                content: selected_input.code_retrieval,
            },
        ];
        Self::compose(total_budget, sections)
    }
}

fn clip_text_to_tokens(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }

    let mut out = String::new();
    for (used, word) in text.split_whitespace().enumerate() {
        if used >= budget {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn estimate_tokens(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Priority levels for context retrieval (Cody approach).
/// Lower numeric values = higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalPriority {
    /// Files directly imported by the target file
    DirectImports = 0,
    /// Type/struct definitions used in the target file
    TypeDefinitions = 1,
    /// Test files for the current module
    TestFiles = 2,
    /// Similar code patterns found in the codebase
    SimilarPatterns = 3,
}

impl RetrievalPriority {
    /// Returns the numeric priority value (lower is higher priority)
    pub const fn value(self) -> u8 {
        self as u8
    }
}

/// Context gathered before an edit (Cody approach).
/// Provides structured context for pre-edit analysis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextFanout {
    /// Live context: current file + direct imports
    pub live_context: Vec<String>,
    /// Indexed context: related files from code search
    pub indexed_context: Vec<String>,
    /// Test context: related test files
    pub test_context: Vec<String>,
    /// Total tokens used
    pub tokens_used: usize,
    /// Files found per priority level
    pub files_by_priority: Vec<(RetrievalPriority, Vec<String>)>,
}

impl ContextFanout {
    /// Gather context for a file about to be edited.
    ///
    /// This implements a priority-based context gathering strategy:
    /// 1. Direct imports (highest priority)
    /// 2. Type definitions
    /// 3. Test files
    /// 4. Similar patterns (lowest priority)
    ///
    /// # Arguments
    /// * `target_file` - The file being edited
    /// * `repo_root` - Root of the repository
    /// * `budget` - Maximum tokens to gather
    ///
    /// # Returns
    /// A `ContextFanout` with gathered context respecting the budget
    pub fn gather(target_file: &Path, repo_root: &Path, budget: usize) -> Result<Self, String> {
        let mut fanout = Self::default();

        // 1. Read target file (live context)
        let target_content = std::fs::read_to_string(target_file)
            .map_err(|e| format!("Failed to read target file: {e}"))?;

        // Estimate tokens for target file
        let target_tokens = estimate_tokens(&target_content);
        fanout.live_context.push(target_file.display().to_string());
        fanout.tokens_used += target_tokens;

        // Early return if budget exceeded by target file alone
        if fanout.tokens_used > budget {
            return Ok(fanout);
        }

        // 2. Parse imports/requires from target file
        let imports = Self::parse_imports(&target_content, target_file);

        // 3. Find imported files (live_context) - Priority: DirectImports
        let remaining = budget.saturating_sub(fanout.tokens_used);
        let (import_files, import_tokens) =
            Self::gather_imported_files(&imports, target_file, repo_root, remaining);
        fanout.live_context.extend(import_files.clone());
        fanout.tokens_used += import_tokens;
        fanout
            .files_by_priority
            .push((RetrievalPriority::DirectImports, import_files));

        // 4. Search for related types/functions (indexed_context) - Priority: TypeDefinitions
        if fanout.tokens_used < budget {
            let remaining = budget.saturating_sub(fanout.tokens_used);
            let (type_files, type_tokens) =
                Self::gather_type_definitions(&target_content, target_file, repo_root, remaining);
            fanout.indexed_context.extend(type_files.clone());
            fanout.tokens_used += type_tokens;
            fanout
                .files_by_priority
                .push((RetrievalPriority::TypeDefinitions, type_files));
        }

        // 5. Find test files for module (test_context) - Priority: TestFiles
        if fanout.tokens_used < budget {
            let remaining = budget.saturating_sub(fanout.tokens_used);
            let (test_files, test_tokens) =
                Self::gather_test_files(target_file, repo_root, remaining);
            fanout.test_context.extend(test_files.clone());
            fanout.tokens_used += test_tokens;
            fanout
                .files_by_priority
                .push((RetrievalPriority::TestFiles, test_files));
        }

        // 6. Similar patterns - Priority: SimilarPatterns (lowest)
        if fanout.tokens_used < budget {
            let remaining = budget.saturating_sub(fanout.tokens_used);
            let (pattern_files, pattern_tokens) =
                Self::gather_similar_patterns(&target_content, target_file, repo_root, remaining);
            fanout.indexed_context.extend(pattern_files.clone());
            fanout.tokens_used += pattern_tokens;
            fanout
                .files_by_priority
                .push((RetrievalPriority::SimilarPatterns, pattern_files));
        }

        Ok(fanout)
    }

    /// Parse imports/requires from a file based on its extension
    fn parse_imports(content: &str, file_path: &Path) -> Vec<String> {
        let extension = file_path.extension().and_then(|e| e.to_str());

        match extension {
            Some("rs") => Self::parse_rust_imports(content),
            Some("ts" | "tsx" | "js" | "jsx") => Self::parse_js_imports(content),
            Some("py") => Self::parse_python_imports(content),
            _ => Vec::new(),
        }
    }

    /// Parse Rust `use` statements and `mod` declarations
    fn parse_rust_imports(content: &str) -> Vec<String> {
        let mut imports = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            // Match `use crate::...;` or `use super::...;` or `use std::...;`
            if line.starts_with("use ") {
                // Extract the import path
                let import_path = line
                    .strip_prefix("use ")
                    .and_then(|s| s.strip_suffix(';'))
                    .unwrap_or("");
                if !import_path.is_empty() {
                    imports.push(import_path.to_string());
                }
            }
            // Match `mod ...;` declarations for local modules
            if line.starts_with("mod ") && line.ends_with(';') {
                let mod_name = line
                    .strip_prefix("mod ")
                    .and_then(|s| s.strip_suffix(';'))
                    .unwrap_or("");
                if !mod_name.is_empty() {
                    imports.push(format!("mod:{mod_name}"));
                }
            }
        }

        imports
    }

    /// Parse JavaScript/TypeScript import statements
    fn parse_js_imports(content: &str) -> Vec<String> {
        let mut imports = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            // Match `import ... from '...';`
            if line.starts_with("import ") {
                if let Some(start) = line.find("from '") {
                    if let Some(end) = line[start + 6..].find('\'') {
                        imports.push(line[start + 6..start + 6 + end].to_string());
                        continue;
                    }
                }
                if let Some(start) = line.find("from \"") {
                    if let Some(end) = line[start + 6..].find('"') {
                        imports.push(line[start + 6..start + 6 + end].to_string());
                    }
                }
            }
            // Match `require('...')`
            if let Some(start) = line.find("require('") {
                if let Some(end) = line[start + 9..].find('\'') {
                    imports.push(line[start + 9..start + 9 + end].to_string());
                }
            }
        }

        imports
    }

    /// Parse Python import statements
    fn parse_python_imports(content: &str) -> Vec<String> {
        let mut imports = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            // Match `import ...`
            if line.starts_with("import ") {
                let import_path = line
                    .strip_prefix("import ")
                    .map_or("", |s| s.split_whitespace().next().unwrap_or(""));
                if !import_path.is_empty() {
                    imports.push(import_path.to_string());
                }
            }
            // Match `from ... import ...`
            if line.starts_with("from ") {
                if let Some(end) = line.find(" import ") {
                    imports.push(line[5..end].to_string());
                }
            }
        }

        imports
    }

    /// Gather imported files based on parsed imports
    fn gather_imported_files(
        imports: &[String],
        target_file: &Path,
        repo_root: &Path,
        budget: usize,
    ) -> (Vec<String>, usize) {
        let mut files = Vec::new();
        let mut tokens_used = 0;

        // Get target directory for relative path resolution
        let target_dir = target_file.parent().unwrap_or_else(|| Path::new("."));

        for import in imports {
            if tokens_used >= budget {
                break;
            }

            // Convert import path to file path
            let file_path = Self::resolve_import_path(import, target_dir, repo_root);

            if let Some(path) = file_path {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let file_tokens = estimate_tokens(&content);
                    if tokens_used + file_tokens <= budget {
                        tokens_used += file_tokens;
                        files.push(path.display().to_string());
                    }
                }
            }
        }

        (files, tokens_used)
    }

    /// Resolve an import path to an actual file path
    fn resolve_import_path(import: &str, target_dir: &Path, repo_root: &Path) -> Option<PathBuf> {
        // Handle Rust module imports
        if import.starts_with("mod:") {
            let mod_name = import.strip_prefix("mod:")?;
            return Some(target_dir.join(format!("{mod_name}.rs")));
        }

        // Handle relative imports (starting with . or ..)
        if import.starts_with('.') || import.starts_with("..") {
            let path = target_dir.join(import);
            return Self::try_extensions(path);
        }

        // Handle crate-relative imports for Rust
        if import.starts_with("crate::") {
            let path = repo_root
                .join("src")
                .join(import.replace("crate::", "").replace("::", "/"));
            return Self::try_extensions(path);
        }

        // Handle JS/TS relative imports
        if import.starts_with("./") || import.starts_with("../") {
            let path = target_dir.join(import);
            return Self::try_extensions(path);
        }

        None
    }

    /// Try adding common file extensions to a path
    fn try_extensions(base: PathBuf) -> Option<PathBuf> {
        let extensions = ["rs", "ts", "tsx", "js", "jsx", "py"];

        // First try without extension
        if base.exists() {
            return Some(base);
        }

        // Try with extensions
        for ext in &extensions {
            let with_ext = base.with_extension(ext);
            if with_ext.exists() {
                return Some(with_ext);
            }
        }

        // Try /mod.rs for Rust
        let mod_rs = base.join("mod.rs");
        if mod_rs.exists() {
            return Some(mod_rs);
        }

        None
    }

    /// Gather type definition files referenced in the target
    fn gather_type_definitions(
        content: &str,
        _target_file: &Path,
        repo_root: &Path,
        budget: usize,
    ) -> (Vec<String>, usize) {
        let mut files = Vec::new();
        let mut tokens_used = 0;

        // Extract type names (simplified heuristic)
        let types = Self::extract_type_names(content);

        // Search for type definitions in the codebase
        for type_name in types {
            if tokens_used >= budget {
                break;
            }

            // Look for files containing this type definition
            if let Some((path, content)) = Self::find_type_definition(&type_name, repo_root) {
                let file_tokens = estimate_tokens(&content);
                if tokens_used + file_tokens <= budget {
                    tokens_used += file_tokens;
                    files.push(path.display().to_string());
                }
            }
        }

        (files, tokens_used)
    }

    /// Extract type names from source code (simplified)
    fn extract_type_names(content: &str) -> Vec<String> {
        let mut types = Vec::new();

        // Rust: struct Foo, enum Bar, impl Baz, pub struct, pub enum
        for line in content.lines() {
            let line = line.trim();
            // Handle "pub struct" and "struct"
            if line.contains(" struct ") {
                let parts = line.split("struct").nth(1).unwrap_or("");
                if let Some(name) = parts.split_whitespace().next() {
                    types.push(name.split('<').next().unwrap_or(name).trim().to_string());
                }
            }
            // Handle "pub enum" and "enum"
            if line.contains(" enum ") {
                let parts = line.split("enum").nth(1).unwrap_or("");
                if let Some(name) = parts.split_whitespace().next() {
                    types.push(name.split('<').next().unwrap_or(name).trim().to_string());
                }
            }
        }

        types.dedup();
        types
    }

    /// Find a file containing a type definition
    fn find_type_definition(type_name: &str, repo_root: &Path) -> Option<(PathBuf, String)> {
        use std::ffi::OsStr;

        // Search in src directory
        let src_dir = repo_root.join("src");

        // Walk through .rs files looking for the type definition
        let entries = std::fs::read_dir(&src_dir).ok()?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension() != Some(OsStr::new("rs")) {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                // Look for struct/enum/mod declarations
                for line in content.lines() {
                    let line = line.trim();
                    let pattern = format!("struct {type_name}");
                    let enum_pattern = format!("enum {type_name}");

                    if line.starts_with(&pattern) || line.starts_with(&enum_pattern) {
                        return Some((path, content));
                    }
                }
            }
        }

        None
    }

    /// Gather test files for the module being edited
    fn gather_test_files(
        target_file: &Path,
        repo_root: &Path,
        budget: usize,
    ) -> (Vec<String>, usize) {
        let mut files = Vec::new();
        let mut tokens_used = 0;

        // Get the module name from the file path
        let file_stem = target_file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        // Keep file stem extraction for future filtering heuristics.
        let _ = file_stem;

        // Also check for tests/ directory
        let tests_dir = repo_root.join("tests");
        if tests_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&tests_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                        continue;
                    }

                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let file_tokens = estimate_tokens(&content);
                        if tokens_used + file_tokens <= budget {
                            tokens_used += file_tokens;
                            files.push(path.display().to_string());
                        }
                    }
                }
            }
        }

        (files, tokens_used)
    }

    /// Gather similar code patterns from the codebase
    fn gather_similar_patterns(
        content: &str,
        target_file: &Path,
        repo_root: &Path,
        budget: usize,
    ) -> (Vec<String>, usize) {
        let mut files = Vec::new();
        let mut tokens_used = 0;

        // Extract function signatures as pattern markers
        let patterns = Self::extract_function_signatures(content);

        for pattern in patterns {
            if tokens_used >= budget {
                break;
            }

            if let Some((path, file_content)) =
                Self::find_similar_pattern(&pattern, target_file, repo_root)
            {
                let file_tokens = estimate_tokens(&file_content);
                if tokens_used + file_tokens <= budget {
                    tokens_used += file_tokens;
                    files.push(path.display().to_string());
                }
            }
        }

        (files, tokens_used)
    }

    /// Extract function signatures for pattern matching
    fn extract_function_signatures(content: &str) -> Vec<String> {
        let mut signatures = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            // Rust: pub fn foo(, fn bar(, etc.
            if line.contains("fn ") && line.contains('(') {
                if let Some(start) = line.find("fn ") {
                    let sig_part = &line[start..];
                    if let Some(end) = sig_part.find('{') {
                        signatures.push(sig_part[..end].to_string());
                    }
                }
            }
        }

        signatures
    }

    /// Find files with similar function patterns
    fn find_similar_pattern(
        pattern: &str,
        exclude_file: &Path,
        repo_root: &Path,
    ) -> Option<(PathBuf, String)> {
        use std::ffi::OsStr;

        let src_dir = repo_root.join("src");
        let entries = std::fs::read_dir(&src_dir).ok()?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path == *exclude_file {
                continue;
            }
            if path.extension() != Some(OsStr::new("rs")) {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                // Simple pattern match: check if any line contains a similar function
                for line in content.lines() {
                    if line.contains("fn ") && line.contains('(') {
                        // Extract function name and compare
                        if let Some(extract_fn) = Self::extract_function_name(pattern) {
                            if line.contains(&extract_fn) {
                                return Some((path, content));
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Extract just the function name from a signature
    fn extract_function_name(signature: &str) -> Option<String> {
        if let Some(start) = signature.find("fn ") {
            let after_fn = &signature[start + 3..];
            if let Some(end) = after_fn.find('(') {
                let name = after_fn[..end].trim();
                return Some(name.to_string());
            }
        }
        None
    }

    /// Check if the fanout has remaining budget for more context
    pub const fn has_remaining_budget(&self, budget: usize) -> bool {
        self.tokens_used < budget
    }

    /// Get the total number of files in the fanout
    pub fn file_count(&self) -> usize {
        self.live_context.len() + self.indexed_context.len() + self.test_context.len()
    }

    /// Get context summary as a formatted string
    pub fn summary(&self) -> String {
        format!(
            "ContextFanout: {} live, {} indexed, {} test files ({} tokens)",
            self.live_context.len(),
            self.indexed_context.len(),
            self.test_context.len(),
            self.tokens_used
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(
        name: &str,
        priority: u8,
        min_tokens: usize,
        max_tokens: usize,
        content: &str,
    ) -> ContextSection {
        ContextSection {
            name: name.to_string(),
            priority,
            min_tokens,
            max_tokens,
            content: content.to_string(),
        }
    }

    #[test]
    fn context_composer_prioritizes_high_priority_sections() {
        let out = ContextBudgetComposer::compose(
            8,
            vec![
                section("low", 1, 1, 8, "one two three four five"),
                section("high", 9, 1, 8, "alpha beta gamma delta"),
            ],
        );
        assert_eq!(out.included_sections[0], "high");
    }

    #[test]
    fn context_composer_tracks_truncated_sections() {
        let out =
            ContextBudgetComposer::compose(4, vec![section("big", 5, 1, 10, "a b c d e f g h")]);
        assert_eq!(out.total_used_tokens, 4);
        assert!(out.truncated_sections.contains(&"big".to_string()));
    }

    #[test]
    fn context_composer_skips_sections_when_minimum_unavailable() {
        let out = ContextBudgetComposer::compose(
            2,
            vec![
                section("high", 9, 2, 2, "a b"),
                section("needs3", 8, 3, 10, "x y z t"),
            ],
        );
        assert_eq!(out.included_sections, vec!["high".to_string()]);
        assert!(out.truncated_sections.contains(&"needs3".to_string()));
    }

    #[test]
    fn retrieval_first_context_keeps_high_signal_sections_first() {
        let out = ContextBudgetComposer::compose_retrieval_first(
            6,
            RetrievalContextInput {
                critical_rules: "r1 r2 r3".to_string(),
                active_task_contract: "c1 c2 c3".to_string(),
                latest_evidence: "e1 e2 e3".to_string(),
                compacted_state_digest: "d1 d2 d3".to_string(),
                code_retrieval: "k1 k2 k3".to_string(),
            },
        );
        assert!(!out.included_sections.is_empty());
        assert_eq!(out.included_sections[0], "critical_rules");
    }

    #[test]
    fn retrieval_first_context_reports_truncation_when_budget_tight() {
        let out = ContextBudgetComposer::compose_retrieval_first(
            3,
            RetrievalContextInput {
                critical_rules: "r1 r2 r3 r4".to_string(),
                active_task_contract: "c1 c2 c3 c4".to_string(),
                latest_evidence: "e1 e2 e3 e4".to_string(),
                compacted_state_digest: "d1 d2 d3 d4".to_string(),
                code_retrieval: "k1 k2 k3 k4".to_string(),
            },
        );
        assert!(out.total_used_tokens <= 3);
        assert!(!out.truncated_sections.is_empty());
    }

    struct FailingAdapter;

    impl RetrievalAdapter for FailingAdapter {
        fn retrieve(
            &self,
            _local: &RetrievalContextInput,
        ) -> Result<RetrievalContextInput, String> {
            Err("adapter unavailable".to_string())
        }
    }

    #[test]
    fn retrieval_first_adapter_failure_falls_back_to_local_input() {
        let local = RetrievalContextInput {
            critical_rules: "local_rule".to_string(),
            active_task_contract: "local_contract".to_string(),
            latest_evidence: "local_evidence".to_string(),
            compacted_state_digest: "local_digest".to_string(),
            code_retrieval: "local_code".to_string(),
        };
        let out = ContextBudgetComposer::compose_retrieval_first_with_adapter(
            10,
            local,
            Some(&FailingAdapter),
        );
        assert!(
            out.rendered.contains("local_rule"),
            "adapter failure should preserve local fallback content"
        );
    }

    // Tests for ContextFanout

    #[test]
    fn context_fanout_default_is_empty() {
        let fanout = ContextFanout::default();
        assert!(fanout.live_context.is_empty());
        assert!(fanout.indexed_context.is_empty());
        assert!(fanout.test_context.is_empty());
        assert_eq!(fanout.tokens_used, 0);
    }

    #[test]
    fn retrieval_priority_ordering_is_correct() {
        assert!(RetrievalPriority::DirectImports < RetrievalPriority::TypeDefinitions);
        assert!(RetrievalPriority::TypeDefinitions < RetrievalPriority::TestFiles);
        assert!(RetrievalPriority::TestFiles < RetrievalPriority::SimilarPatterns);
    }

    #[test]
    fn retrieval_priority_values_are_correct() {
        assert_eq!(RetrievalPriority::DirectImports.value(), 0);
        assert_eq!(RetrievalPriority::TypeDefinitions.value(), 1);
        assert_eq!(RetrievalPriority::TestFiles.value(), 2);
        assert_eq!(RetrievalPriority::SimilarPatterns.value(), 3);
    }

    #[test]
    fn parse_rust_imports_extracts_use_statements() {
        let content = r"
use std::collections::HashMap;
use crate::runtime::reliability::state::RuntimeState;
use super::lease::LeaseManager;
";
        let imports = ContextFanout::parse_rust_imports(content);
        assert_eq!(imports.len(), 3);
        assert!(
            imports
                .iter()
                .any(|i| i.contains("std::collections::HashMap"))
        );
        assert!(
            imports
                .iter()
                .any(|i| i.contains("crate::runtime::reliability::state"))
        );
        assert!(imports.iter().any(|i| i.contains("super::lease")));
    }

    #[test]
    fn parse_rust_imports_extracts_mod_declarations() {
        let content = r"
mod lease;
mod state;
pub mod context_budget;
";
        let imports = ContextFanout::parse_rust_imports(content);
        // Only private mod declarations are extracted (pub mod is handled differently)
        assert!(imports.len() >= 2);
        assert!(imports.iter().any(|i| i == "mod:lease"));
        assert!(imports.iter().any(|i| i == "mod:state"));
    }

    #[test]
    fn context_fanout_respects_budget() {
        // This test creates a minimal temp file and tests budget behavior
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let repo_root = temp_dir.path();
        let src_dir = repo_root.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src");

        let target_file = src_dir.join("test.rs");
        std::fs::write(
            &target_file,
            "pub struct TestStruct { field: i32 }\n\nimpl TestStruct {\n    pub fn new() -> Self { Self { field: 0 } }\n}\n"
        ).expect("write test file");

        // Very small budget - should only fit target file
        let fanout =
            ContextFanout::gather(&target_file, repo_root, 5).expect("gather should succeed");

        // Should have at least the target file
        assert!(!fanout.live_context.is_empty());
        // Budget check: tokens_used will be >=5 since we always include target file
        // The key is that we don't gather additional context beyond the budget
        assert!(
            fanout.file_count() <= 2,
            "should only have target file, maybe imports"
        );
    }

    #[test]
    fn extract_type_names_finds_structs_and_enums() {
        let content = r"
pub struct MyStruct {
    field: i32,
}

pub enum MyEnum {
    VariantA,
    VariantB,
}

struct PrivateStruct(u8);
";
        let types = ContextFanout::extract_type_names(content);
        // Check that we find the expected types (implementation extracts the name after the type keyword)
        assert!(
            types
                .iter()
                .any(|t| t.contains("MyStruct") || t == "MyStruct")
        );
        assert!(types.iter().any(|t| t.contains("MyEnum") || t == "MyEnum"));
    }

    #[test]
    fn extract_function_signatures_finds_fn_declarations() {
        let content = r"
pub fn public_function(x: i32) -> i32 {
    x + 1
}

fn private_function() -> Result<(), Error> {
    Ok(())
}
";
        let sigs = ContextFanout::extract_function_signatures(content);
        assert_eq!(sigs.len(), 2);
        assert!(sigs.iter().any(|s| s.contains("public_function")));
        assert!(sigs.iter().any(|s| s.contains("private_function")));
    }

    #[test]
    fn extract_function_name_gets_identifier() {
        assert_eq!(
            ContextFanout::extract_function_name("fn my_function(x: i32)"),
            Some("my_function".to_string())
        );
        // Generic functions include the <T> part - this is expected behavior
        assert_eq!(
            ContextFanout::extract_function_name("pub fn complex_func(t: T) -> T"),
            Some("complex_func".to_string())
        );
    }

    #[test]
    fn context_fanout_file_count_is_accurate() {
        let fanout = ContextFanout {
            live_context: vec!["a.rs".to_string(), "b.rs".to_string()],
            indexed_context: vec!["c.rs".to_string()],
            test_context: vec!["a_test.rs".to_string()],
            ..ContextFanout::default()
        };

        assert_eq!(fanout.file_count(), 4);
    }

    #[test]
    fn context_fanout_summary_formats_correctly() {
        let fanout = ContextFanout {
            live_context: vec!["main.rs".to_string()],
            indexed_context: vec!["helper.rs".to_string()],
            test_context: vec!["main_test.rs".to_string()],
            tokens_used: 1234,
            ..ContextFanout::default()
        };

        let summary = fanout.summary();
        assert!(summary.contains("1 live"));
        assert!(summary.contains("1 indexed"));
        assert!(summary.contains("1 test"));
        assert!(summary.contains("1234 tokens"));
    }

    #[test]
    fn context_fanout_has_remaining_budget_checks_correctly() {
        let fanout = ContextFanout {
            tokens_used: 50,
            ..ContextFanout::default()
        };

        assert!(fanout.has_remaining_budget(100));
        assert!(!fanout.has_remaining_budget(50));
        assert!(!fanout.has_remaining_budget(25));
    }
}
