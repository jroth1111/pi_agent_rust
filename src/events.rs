//! Typed Action/Observation system for tool execution.
//!
//! This module provides an OpenHands-style strongly-typed action/observation pattern
//! for tool execution, replacing generic JSON-based tool calls with type-safe enums.
//!
//! # Overview
//!
//! The system consists of three main types:
//!
//! - [`Action`]: Enum representing all possible tool operations with typed parameters
//! - [`Observation`]: Enum representing structured results from action execution
//! - [`ActionResult`]: Pairs an action with its observation for logging/replay
//!
//! # Example
//!
//! ```rust
//! use pi::events::{Action, Observation, ActionResult};
//!
//! // Create an action using the builder API
//! let action = Action::read("/path/to/file.rs");
//!
//! // After execution, create an observation
//! let observation = Observation::Read(ReadObservation {
//!     file_path: "/path/to/file.rs".to_string(),
//!     content: "fn main() {}".to_string(),
//!     line_info: None,
//!     truncated: false,
//!     truncation: None,
//!     metadata: None,
//! });
//!
//! // Pair them into a result
//! let result = ActionResult::new(action, observation);
//! ```
//!
//! # Action Variants
//!
//! | Variant | Description | Key Fields |
//! |---------|-------------|------------|
//! | `Read` | Read file contents | `file_path`, `offset`, `limit` |
//! | `Write` | Write to file | `file_path`, `content` |
//! | `Edit` | Replace text in file | `file_path`, `old_string`, `new_string` |
//! | `Bash` | Execute shell command | `command`, `cwd`, `timeout` |
//! | `Grep` | Search file contents | `pattern`, `path`, `glob` |
//! | `Glob` | Find files by pattern | `pattern`, `path` |
//! | `Ls` | List directory | `path`, `limit` |
//! | `WebSearch` | Search the web | `query`, `limit` |
//! | `WebFetch` | Fetch web page | `url`, `timeout` |
//! | `Lsp` | LSP operation | `operation`, `file_path` |
//!
//! # Benefits
//!
//! - **Compile-time safety**: Invalid actions cannot be constructed
//! - **IDE support**: Auto-completion and type hints for all actions
//! - **Refactoring safety**: Changes to actions are caught at compile time
//! - **Self-documenting**: Action variants clearly show available operations
//! - **Serializable**: All types support serde for persistence/transmission
//!
//! This module provides an OpenHands-style strongly-typed action/observation pattern
//! for tool execution, replacing generic JSON-based tool calls with type-safe enums.
//!
//! ## Design
//!
//! - [`Action`] represents a typed request to perform an operation
//! - [`Observation`] represents the structured result of an action
//! - [`ActionResult`] pairs an action with its observation
//!
//! ## Motivation
//!
//! Strong typing provides:
//! - **Compile-time safety**: Invalid actions cannot be constructed
//! - **IDE support**: Auto-completion and type hints for actions
//! - **Refactoring safety**: Changes to actions are caught at compile time
//! - **Self-documentation**: Action variants clearly show available operations
//!
//! ## Interoperability
//!
//! The legacy [`Tool`](crate::tools::Tool) trait is maintained via `From`
//! implementations and structured serialization rather than content-block
//! adapters.

use crate::model::ContentBlock;
use crate::tools::TruncationResult;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ============================================================================
// Action Enum
// ============================================================================

/// A typed action that can be executed by the agent.
///
/// Each variant represents a specific tool operation with strongly-typed parameters.
/// This replaces generic JSON-based tool calls with compile-time type safety.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "actionType", rename_all = "camelCase")]
pub enum Action {
    /// Read file contents.
    Read(ReadAction),

    /// Write content to a file (creates or overwrites).
    Write(WriteAction),

    /// Edit a file with precise string replacement.
    Edit(EditAction),

    /// Execute a shell command.
    Bash(BashAction),

    /// Search file contents with regex patterns.
    Grep(GrepAction),

    /// Find files by glob pattern.
    Glob(GlobAction),

    /// List directory contents.
    Ls(LsAction),

    /// Search the web.
    WebSearch(WebSearchAction),

    /// Fetch and parse a web page.
    WebFetch(WebFetchAction),

    /// Language Server Protocol operation.
    Lsp(LspAction),

    /// Custom/extension action.
    Custom {
        /// The tool name
        name: String,
        /// The raw arguments (for tools not yet typed)
        arguments: serde_json::Value,
    },
}

// ============================================================================
// File Actions
// ============================================================================

/// Action to read file contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadAction {
    /// Absolute or relative path to the file.
    pub file_path: String,

    /// Optional starting line offset (1-based, inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,

    /// Optional maximum number of lines to read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Action to write content to a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteAction {
    /// Absolute or relative path to the file.
    pub file_path: String,

    /// The content to write.
    pub content: String,
}

/// Action to edit a file with string replacement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditAction {
    /// Absolute or relative path to the file.
    pub file_path: String,

    /// The exact text to replace.
    pub old_string: String,

    /// The replacement text.
    pub new_string: String,

    /// Replace all occurrences instead of just the first.
    #[serde(default)]
    pub replace_all: bool,
}

// ============================================================================
// Shell Actions
// ============================================================================

/// Action to execute a shell command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashAction {
    /// The command to execute.
    pub command: String,

    /// Current working directory for the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    /// Timeout in milliseconds (0 = no timeout).
    #[serde(default = "default_timeout")]
    pub timeout: u64,

    /// Environment variables to set.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,

    /// Whether to stream output during execution.
    #[serde(default)]
    pub stream: bool,
}

const fn default_timeout() -> u64 {
    120_000 // 2 minutes default
}

// ============================================================================
// Search Actions
// ============================================================================

/// Action to search file contents with regex.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepAction {
    /// Regular expression pattern to search for.
    pub pattern: String,

    /// Directory to search in (default: current directory).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Optional glob filter (e.g. "*.rs").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,

    /// Output mode: "content", "files_with_matches", or "count".
    #[serde(default = "default_grep_mode")]
    pub output_mode: GrepOutputMode,

    /// Case-insensitive search.
    #[serde(default)]
    pub ignore_case: bool,

    /// Maximum number of results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    /// Context lines to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
}

/// Grep output mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrepOutputMode {
    /// Show matching lines with context.
    #[default]
    Content,
    /// Show only file paths containing matches.
    FilesWithMatches,
    /// Show match count per file.
    Count,
}

const fn default_grep_mode() -> GrepOutputMode {
    GrepOutputMode::Content
}

/// Action to find files by glob pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobAction {
    /// Glob pattern (e.g. "**/*.rs").
    pub pattern: String,

    /// Directory to search in (default: current directory).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

// ============================================================================
// Directory Actions
// ============================================================================

/// Action to list directory contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsAction {
    /// Directory path to list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Maximum number of entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    /// Show detailed file info.
    #[serde(default)]
    pub detailed: bool,
}

// ============================================================================
// Web Actions
// ============================================================================

/// Action to search the web.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchAction {
    /// Search query.
    pub query: String,

    /// Maximum number of results.
    #[serde(default = "default_web_limit")]
    pub limit: usize,

    /// Recency filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recency: Option<WebRecency>,
}

/// Web search recency filter.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebRecency {
    OneDay,
    OneWeek,
    OneMonth,
    OneYear,
    NoLimit,
}

const fn default_web_limit() -> usize {
    5
}

/// Action to fetch and parse a web page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchAction {
    /// URL to fetch.
    pub url: String,

    /// Maximum timeout in seconds.
    #[serde(default = "default_web_timeout")]
    pub timeout: u64,
}

const fn default_web_timeout() -> u64 {
    20
}

// ============================================================================
// LSP Actions
// ============================================================================

/// Action for Language Server Protocol operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspAction {
    /// LSP operation type.
    pub operation: LspOperation,

    /// File path to operate on.
    pub file_path: String,

    /// Optional position (line, column).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<LspPosition>,
}

/// LSP operation types.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LspOperation {
    /// Go to definition.
    GoToDefinition,
    /// Find references.
    FindReferences,
    /// Hover information.
    Hover,
    /// Document symbols.
    DocumentSymbols,
    /// Completion.
    Completion,
}

/// Position in a text document (LSP 0-based).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspPosition {
    /// Line number (0-based).
    pub line: u32,

    /// Character offset (0-based).
    pub character: u32,
}

// ============================================================================
// Observation Enum
// ============================================================================

/// A typed observation resulting from an action execution.
///
/// Each variant corresponds to the result of an [`Action`] variant,
/// providing structured, typed response data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "observationType", rename_all = "camelCase")]
pub enum Observation {
    /// Result of reading a file.
    Read(ReadObservation),

    /// Result of writing a file.
    Write(WriteObservation),

    /// Result of editing a file.
    Edit(EditObservation),

    /// Result of executing a bash command.
    Bash(BashObservation),

    /// Result of a grep search.
    Grep(GrepObservation),

    /// Result of a glob search.
    Glob(GlobObservation),

    /// Result of listing a directory.
    Ls(LsObservation),

    /// Result of a web search.
    WebSearch(WebSearchObservation),

    /// Result of fetching a web page.
    WebFetch(WebFetchObservation),

    /// Result of an LSP operation.
    Lsp(LspObservation),

    /// Error observation.
    Error {
        /// Error message.
        message: String,
        /// The action that caused the error.
        action: Box<Action>,
    },

    /// Custom/extension observation.
    Custom {
        /// The tool name.
        name: String,
        /// The result content.
        content: Vec<ContentBlock>,
        /// Optional metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        /// Whether this is an error.
        #[serde(default)]
        is_error: bool,
    },
}

// ============================================================================
// File Observations
// ============================================================================

/// Observation from reading a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadObservation {
    /// The file that was read.
    pub file_path: String,

    /// The content that was read.
    pub content: String,

    /// Line number info (if offset/limit were used).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_info: Option<LineInfo>,

    /// Whether the content was truncated.
    #[serde(default)]
    pub truncated: bool,

    /// Truncation details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationInfo>,

    /// File metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<FileMetadata>,
}

/// Line number information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineInfo {
    /// First line number (1-based).
    pub first_line: usize,
    /// Last line number (1-based).
    pub last_line: usize,
    /// Total lines in the file.
    pub total_lines: usize,
}

/// Truncation information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationInfo {
    /// What caused truncation.
    pub truncated_by: TruncatedBy,
    /// Total lines before truncation.
    pub total_lines: usize,
    /// Total bytes before truncation.
    pub total_bytes: usize,
    /// Output lines after truncation.
    pub output_lines: usize,
    /// Output bytes after truncation.
    pub output_bytes: usize,
}

/// What caused truncation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TruncatedBy {
    /// Truncated by line count.
    Lines,
    /// Truncated by byte count.
    Bytes,
}

impl From<crate::tools::TruncatedBy> for TruncatedBy {
    fn from(value: crate::tools::TruncatedBy) -> Self {
        match value {
            crate::tools::TruncatedBy::Lines => Self::Lines,
            crate::tools::TruncatedBy::Bytes => Self::Bytes,
        }
    }
}

/// File metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadata {
    /// File size in bytes.
    pub size: u64,

    /// MIME type (if detected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,

    /// Last modified timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
}

/// Observation from writing a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteObservation {
    /// The file that was written.
    pub file_path: String,

    /// Number of bytes written.
    pub bytes_written: usize,

    /// Whether a new file was created.
    #[serde(default)]
    pub created: bool,

    /// File metadata after write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<FileMetadata>,
}

/// Observation from editing a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditObservation {
    /// The file that was edited.
    pub file_path: String,

    /// Number of replacements made.
    pub replacements: usize,

    /// Optional diff summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffSummary>,
}

/// Summary of edits made.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    /// Number of lines added.
    pub lines_added: usize,

    /// Number of lines removed.
    pub lines_removed: usize,
}

// ============================================================================
// Shell Observations
// ============================================================================

/// Observation from executing a bash command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashObservation {
    /// The command that was executed.
    pub command: String,

    /// Working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    /// Command output (stdout + stderr combined).
    pub output: String,

    /// Exit code.
    pub exit_code: Option<i32>,

    /// Whether the command timed out.
    #[serde(default)]
    pub timed_out: bool,

    /// Duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,

    /// Whether the output was truncated.
    #[serde(default)]
    pub truncated: bool,

    /// Truncation info.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationInfo>,
}

// ============================================================================
// Search Observations
// ============================================================================

/// Observation from a grep search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepObservation {
    /// The pattern that was searched for.
    pub pattern: String,

    /// Directory that was searched.
    pub path: String,

    /// Type of results returned.
    pub output_mode: GrepOutputMode,

    /// Matching results (content mode).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<GrepMatch>,

    /// Files with matches (files_with_matches mode).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,

    /// Match counts (count mode).
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub counts: HashMap<String, usize>,

    /// Total matches found (may be more than returned due to limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_matches: Option<usize>,

    /// Whether results were truncated.
    #[serde(default)]
    pub truncated: bool,
}

/// A single grep match.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepMatch {
    /// File path containing the match.
    pub file: String,

    /// Line number (1-based).
    pub line_number: usize,

    /// The matching line content.
    pub line: String,

    /// The matched text (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_text: Option<String>,

    /// Context lines before the match.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_before: Vec<String>,

    /// Context lines after the match.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_after: Vec<String>,
}

/// Observation from a glob search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobObservation {
    /// The glob pattern.
    pub pattern: String,

    /// Directory searched.
    pub path: String,

    /// Matching file paths.
    pub matches: Vec<String>,

    /// Total matches found.
    pub total_count: usize,
}

// ============================================================================
// Directory Observations
// ============================================================================

/// Observation from listing a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsObservation {
    /// The directory that was listed.
    pub path: String,

    /// Directory entries.
    pub entries: Vec<DirEntry>,

    /// Whether the listing was truncated.
    #[serde(default)]
    pub truncated: bool,
}

/// A directory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    /// Entry name.
    pub name: String,

    /// Full path.
    pub path: String,

    /// Entry type.
    #[serde(rename = "type")]
    pub entry_type: DirEntryType,

    /// Size in bytes (files only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,

    /// Last modified timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
}

/// Directory entry type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirEntryType {
    File,
    Directory,
    Symlink,
}

// ============================================================================
// Web Observations
// ============================================================================

/// Observation from a web search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchObservation {
    /// The query that was searched.
    pub query: String,

    /// Search results.
    pub results: Vec<WebSearchResult>,

    /// Total results available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<usize>,
}

/// A single web search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResult {
    /// Result title.
    pub title: String,

    /// Result URL.
    pub url: String,

    /// Result snippet/summary.
    pub snippet: String,

    /// Source website name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Favicon URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// Observation from fetching a web page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchObservation {
    /// The URL that was fetched.
    pub url: String,

    /// Page content (markdown/text).
    pub content: String,

    /// Page title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Content length in characters.
    pub content_length: usize,

    /// Whether the content was truncated.
    #[serde(default)]
    pub truncated: bool,

    /// Fetch duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

// ============================================================================
// LSP Observations
// ============================================================================

/// Observation from an LSP operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspObservation {
    /// The operation performed.
    pub operation: LspOperation,

    /// The file that was operated on.
    pub file_path: String,

    /// Operation result (structured).
    #[serde(flatten)]
    pub result: LspResult,
}

/// LSP operation results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resultType", rename_all = "camelCase")]
pub enum LspResult {
    /// Definition locations.
    Definition(Vec<LspLocation>),

    /// Reference locations.
    References(Vec<LspLocation>),

    /// Hover information.
    Hover {
        /// Hover text/markdown.
        content: String,
        /// Optional range.
        range: Option<LspRange>,
    },

    /// Document symbols.
    Symbols(Vec<LspSymbol>),

    /// Completion items.
    Completions(Vec<LspCompletionItem>),

    /// Error result.
    Error {
        /// Error message.
        message: String,
    },
}

/// A location in a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspLocation {
    /// File path.
    pub uri: String,

    /// Position range.
    pub range: LspRange,
}

/// A range in a text document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspRange {
    /// Start position.
    pub start: LspPosition,

    /// End position.
    pub end: LspPosition,
}

/// A document symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspSymbol {
    /// Symbol name.
    pub name: String,

    /// Symbol kind (function, class, etc.).
    pub kind: String,

    /// Symbol location.
    pub range: LspRange,

    /// Child symbols.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Self>,
}

/// A completion item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspCompletionItem {
    /// Label text.
    pub label: String,

    /// Detail text (type, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    /// Documentation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,

    /// Kind (function, variable, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

// ============================================================================
// ActionResult
// ============================================================================

/// Pairs an [`Action`] with its resulting [`Observation`].
///
/// This is the primary type returned from tool execution, providing
/// both the request and its response for logging, replay, and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    /// The action that was executed.
    pub action: Action,

    /// The observation/result from executing the action.
    pub observation: Observation,

    /// Execution timestamp.
    pub timestamp: i64,

    /// Execution duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl ActionResult {
    /// Create a new action result.
    pub fn new(action: Action, observation: Observation) -> Self {
        Self {
            action,
            observation,
            timestamp: chrono_timestamp(),
            duration_ms: None,
        }
    }

    /// Create a new action result with duration.
    pub fn with_duration(action: Action, observation: Observation, duration_ms: u64) -> Self {
        Self {
            action,
            observation,
            timestamp: chrono_timestamp(),
            duration_ms: Some(duration_ms),
        }
    }

    /// Create an error result.
    pub fn error(action: Action, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            action: action.clone(),
            observation: Observation::Error {
                message,
                action: Box::new(action),
            },
            timestamp: chrono_timestamp(),
            duration_ms: None,
        }
    }

    /// Check if the result is an error.
    pub const fn is_error(&self) -> bool {
        matches!(
            self.observation,
            Observation::Error { .. } | Observation::Custom { is_error: true, .. }
        )
    }
}

/// Get current Unix timestamp in milliseconds.
fn chrono_timestamp() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ============================================================================
// Conversions from legacy types
// ============================================================================

impl From<TruncationResult> for TruncationInfo {
    fn from(value: TruncationResult) -> Self {
        Self {
            truncated_by: value.truncated_by.map_or(TruncatedBy::Lines, Into::into),
            total_lines: value.total_lines,
            total_bytes: value.total_bytes,
            output_lines: value.output_lines,
            output_bytes: value.output_bytes,
        }
    }
}

// ============================================================================
// Action builders (convenience API)
// ============================================================================

/// Builder-style API for constructing actions.
impl Action {
    /// Create a read action.
    pub fn read(file_path: impl Into<String>) -> Self {
        Self::Read(ReadAction {
            file_path: file_path.into(),
            offset: None,
            limit: None,
        })
    }

    /// Create a read action with offset/limit.
    pub fn read_range(file_path: impl Into<String>, offset: usize, limit: usize) -> Self {
        Self::Read(ReadAction {
            file_path: file_path.into(),
            offset: Some(offset),
            limit: Some(limit),
        })
    }

    /// Create a write action.
    pub fn write(file_path: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Write(WriteAction {
            file_path: file_path.into(),
            content: content.into(),
        })
    }

    /// Create an edit action.
    pub fn edit(
        file_path: impl Into<String>,
        old: impl Into<String>,
        new: impl Into<String>,
    ) -> Self {
        Self::Edit(EditAction {
            file_path: file_path.into(),
            old_string: old.into(),
            new_string: new.into(),
            replace_all: false,
        })
    }

    /// Create a bash action.
    pub fn bash(command: impl Into<String>) -> Self {
        Self::Bash(BashAction {
            command: command.into(),
            cwd: None,
            timeout: default_timeout(),
            env: HashMap::new(),
            stream: false,
        })
    }

    /// Create a grep action.
    pub fn grep(pattern: impl Into<String>) -> Self {
        Self::Grep(GrepAction {
            pattern: pattern.into(),
            path: None,
            glob: None,
            output_mode: default_grep_mode(),
            ignore_case: false,
            limit: None,
            context: None,
        })
    }

    /// Create a glob action.
    pub fn glob(pattern: impl Into<String>) -> Self {
        Self::Glob(GlobAction {
            pattern: pattern.into(),
            path: None,
        })
    }

    /// Create an ls action.
    pub const fn ls() -> Self {
        Self::Ls(LsAction {
            path: None,
            limit: None,
            detailed: false,
        })
    }

    /// Create a web search action.
    pub fn web_search(query: impl Into<String>) -> Self {
        Self::WebSearch(WebSearchAction {
            query: query.into(),
            limit: default_web_limit(),
            recency: None,
        })
    }

    /// Create a web fetch action.
    pub fn web_fetch(url: impl Into<String>) -> Self {
        Self::WebFetch(WebFetchAction {
            url: url.into(),
            timeout: default_web_timeout(),
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_serialization() {
        let action = Action::read("/path/to/file.rs");
        let json = serde_json::to_string(&action).unwrap();
        let parsed: Action = serde_json::from_str(&json).unwrap();

        match parsed {
            Action::Read(read) => {
                assert_eq!(read.file_path, "/path/to/file.rs");
                assert!(read.offset.is_none());
                assert!(read.limit.is_none());
            }
            _ => panic!("Expected Read action"),
        }
    }

    #[test]
    fn test_action_result_roundtrip() {
        let action = Action::read("/test.rs");
        let observation = Observation::Read(ReadObservation {
            file_path: "/test.rs".to_string(),
            content: "fn main() {}".to_string(),
            line_info: None,
            truncated: false,
            truncation: None,
            metadata: None,
        });

        let result = ActionResult::new(action.clone(), observation.clone());
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ActionResult = serde_json::from_str(&json).unwrap();

        match (parsed.action, parsed.observation) {
            (Action::Read(read), Observation::Read(obs)) => {
                assert_eq!(read.file_path, "/test.rs");
                assert_eq!(obs.content, "fn main() {}");
            }
            _ => panic!("Roundtrip failed"),
        }
    }

    #[test]
    fn test_action_builder_api() {
        let read = Action::read("/test.rs");
        let write = Action::write("/test.rs", "content");
        let edit = Action::edit("/test.rs", "old", "new");
        let bash = Action::bash("echo hello");
        let grep = Action::grep("pattern");
        let glob = Action::glob("**/*.rs");

        // Verify action types
        assert!(matches!(read, Action::Read(_)));
        assert!(matches!(write, Action::Write(_)));
        assert!(matches!(edit, Action::Edit(_)));
        assert!(matches!(bash, Action::Bash(_)));
        assert!(matches!(grep, Action::Grep(_)));
        assert!(matches!(glob, Action::Glob(_)));
    }

    #[test]
    fn test_error_observation() {
        let action = Action::read("/nonexistent.rs");
        let result = ActionResult::error(action, "File not found");

        assert!(result.is_error());
        match &result.observation {
            Observation::Error { message, .. } => {
                assert_eq!(message, "File not found");
            }
            _ => panic!("Expected Error observation"),
        }
    }

    #[test]
    fn test_grep_output_mode() {
        let mode = GrepOutputMode::Content;
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: GrepOutputMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, GrepOutputMode::Content);
    }

    #[test]
    fn test_custom_action() {
        let action = Action::Custom {
            name: "my_tool".to_string(),
            arguments: serde_json::json!({"key": "value"}),
        };

        let json = serde_json::to_string(&action).unwrap();
        let parsed: Action = serde_json::from_str(&json).unwrap();

        match parsed {
            Action::Custom { name, arguments } => {
                assert_eq!(name, "my_tool");
                assert_eq!(arguments, serde_json::json!({"key": "value"}));
            }
            _ => panic!("Expected Custom action"),
        }
    }
}
