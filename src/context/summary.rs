//! Structured summaries for context compaction.
//!
//! This module implements the OpenHands approach for better compaction quality
//! by providing structured summary types that capture:
//! - User intent and goals
//! - Task progress (completed, pending)
//! - File operations (examined, modified)
//! - Key decisions and their rationale
//! - Error history and resolutions
//!
//! These structured summaries can be serialized to JSON for storage and
//! formatted as human-readable context strings for LLM consumption.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Type of file change operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeType {
    /// File was created.
    Created,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
}

impl std::fmt::Display for ChangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Modified => write!(f, "modified"),
            Self::Deleted => write!(f, "deleted"),
        }
    }
}

/// Summary of a file that was examined during the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSummary {
    /// Path to the file.
    pub path: String,
    /// Brief description of the file's purpose.
    pub purpose: String,
    /// Key insights or findings from examining this file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_insights: Vec<String>,
}

/// Record of a file change operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// Path to the file.
    pub path: String,
    /// Type of change (created, modified, deleted).
    pub change_type: ChangeType,
    /// Summary of what was changed.
    pub summary: String,
}

/// Outcome of a completed or attempted task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutcome {
    /// Description of the task.
    pub task: String,
    /// Result status: "success", "partial", or "failed".
    pub result: String,
    /// Additional details about the outcome.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub details: String,
}

/// A key decision made during the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    /// The decision that was made.
    pub decision: String,
    /// Rationale explaining why this decision was made.
    pub rationale: String,
}

/// Record of an error encountered during the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorRecord {
    /// The error message or description.
    pub error: String,
    /// How the error was resolved, if applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

/// Snapshot of the runtime orchestration state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrationSummary {
    /// Runtime objective currently being executed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub objective: String,
    /// High-level runtime phase.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
    /// Active blockers reported by the runtime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Recent runtime actions worth preserving across compaction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_actions: Vec<String>,
    /// Next action suggested by the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
}

impl OrchestrationSummary {
    pub fn is_empty(&self) -> bool {
        self.objective.is_empty()
            && self.phase.is_empty()
            && self.blockers.is_empty()
            && self.recent_actions.is_empty()
            && self.next_action.is_none()
    }
}

/// Verification state attached to a runtime task.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VerificationSummary {
    /// Verification command that last ran for the task.
    pub command: String,
    /// Verification outcome status.
    pub status: String,
    /// Exit code when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Dense task snapshot derived from the reliability runtime.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeTaskSummary {
    /// Stable runtime task identifier.
    pub task_id: String,
    /// Current runtime state label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    /// Task objective or title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub objective: String,
    /// Current blockers tied to the task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Most recent noteworthy update.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub latest_update: String,
    /// Last checkpoint phase recorded for the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checkpoint_phase: Option<String>,
    /// Latest verification status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationSummary>,
    /// Outcome recorded during close-out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_outcome: Option<String>,
}

impl RuntimeTaskSummary {
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.task_id.is_empty()
            && self.state.is_empty()
            && self.objective.is_empty()
            && self.blockers.is_empty()
            && self.latest_update.is_empty()
            && self.last_checkpoint_phase.is_none()
            && self.verification.is_none()
            && self.close_outcome.is_none()
    }
}

/// Structured summary for context compaction.
///
/// This is the main type for capturing session state in a structured format
/// that can be easily consumed by an LLM when continuing a session after
/// compaction.
///
/// # Example
///
/// ```
/// use pi::context::summary::{CompactionSummary, TaskOutcome, Decision};
///
/// let summary = CompactionSummary {
///     user_intent: "Fix the authentication bug in login.rs".to_string(),
///     completed_tasks: vec![
///         TaskOutcome {
///             task: "Identified the root cause".to_string(),
///             result: "success".to_string(),
///             details: "Missing token validation".to_string(),
///         },
///     ],
///     pending_tasks: vec!["Write unit tests".to_string()],
///     files_examined: vec![],
///     files_modified: vec![],
///     current_state: "Bug fixed, tests pending".to_string(),
///     key_decisions: vec![
///         Decision {
///             decision: "Use JWT validation".to_string(),
///             rationale: "More secure than session-based auth".to_string(),
///         },
///     ],
///     error_history: vec![],
///     ..Default::default()
/// };
///
/// let context_string = summary.to_context_string();
/// assert!(context_string.contains("User Intent"));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummary {
    /// What the user is trying to accomplish.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_intent: String,

    /// Tasks that have been completed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_tasks: Vec<TaskOutcome>,

    /// Tasks that are still pending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_tasks: Vec<String>,

    /// Files that were examined (read-only access).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_examined: Vec<FileSummary>,

    /// Files that were modified (created, edited, or deleted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_modified: Vec<FileChange>,

    /// Current state of the work.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub current_state: String,

    /// Runtime orchestration state preserved across compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<OrchestrationSummary>,

    /// Runtime task snapshots preserved across compaction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_tasks: Vec<RuntimeTaskSummary>,

    /// Key decisions made during the session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_decisions: Vec<Decision>,

    /// History of errors encountered and their resolutions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub error_history: Vec<ErrorRecord>,
}

impl CompactionSummary {
    /// Format the summary as a human-readable string for LLM context.
    ///
    /// This produces a structured markdown-like format that is easy for
    /// an LLM to parse and understand when continuing a session.
    pub fn to_context_string(&self) -> String {
        let mut output = String::new();

        // User Intent
        if !self.user_intent.is_empty() {
            output.push_str("## User Intent\n");
            output.push_str(&self.user_intent);
            output.push_str("\n\n");
        }

        // Completed Tasks
        if !self.completed_tasks.is_empty() {
            output.push_str("## Completed Tasks\n");
            for task in &self.completed_tasks {
                let status_icon = match task.result.as_str() {
                    "success" => "[x]",
                    "partial" => "[~]",
                    _ => "[!]",
                };
                let _ = writeln!(output, "- {status_icon} {} ({})", task.task, task.result);
                if !task.details.is_empty() {
                    let _ = writeln!(output, "  - {}", task.details);
                }
            }
            output.push('\n');
        }

        // Pending Tasks
        if !self.pending_tasks.is_empty() {
            output.push_str("## Pending Tasks\n");
            for task in &self.pending_tasks {
                let _ = writeln!(output, "- [ ] {task}");
            }
            output.push('\n');
        }

        // Files Examined
        if !self.files_examined.is_empty() {
            output.push_str("## Files Examined\n");
            for file in &self.files_examined {
                let _ = writeln!(output, "- `{}`: {}", file.path, file.purpose);
                for insight in &file.key_insights {
                    let _ = writeln!(output, "  - {insight}");
                }
            }
            output.push('\n');
        }

        // Files Modified
        if !self.files_modified.is_empty() {
            output.push_str("## Files Modified\n");
            for change in &self.files_modified {
                let _ = writeln!(
                    output,
                    "- `{}` ({}): {}",
                    change.path, change.change_type, change.summary
                );
            }
            output.push('\n');
        }

        // Current State
        if !self.current_state.is_empty() {
            output.push_str("## Current State\n");
            output.push_str(&self.current_state);
            output.push_str("\n\n");
        }

        if let Some(orchestration) = &self.orchestration {
            if !orchestration.is_empty() {
                output.push_str("## Orchestration\n");
                if !orchestration.phase.is_empty() || !orchestration.objective.is_empty() {
                    if orchestration.phase.is_empty() {
                        let _ = writeln!(output, "- objective: {}", orchestration.objective);
                    } else if orchestration.objective.is_empty() {
                        let _ = writeln!(output, "- phase: {}", orchestration.phase);
                    } else {
                        let _ = writeln!(
                            output,
                            "- phase: {} | objective: {}",
                            orchestration.phase, orchestration.objective
                        );
                    }
                }
                if let Some(next_action) = &orchestration.next_action {
                    let _ = writeln!(output, "- next: {}", next_action);
                }
                for blocker in &orchestration.blockers {
                    let _ = writeln!(output, "- blocker: {}", blocker);
                }
                for action in &orchestration.recent_actions {
                    let _ = writeln!(output, "- recent: {}", action);
                }
                output.push('\n');
            }
        }

        if !self.runtime_tasks.is_empty() {
            output.push_str("## Runtime Tasks\n");
            let mut tasks = self.runtime_tasks.clone();
            tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
            for task in &tasks {
                let _ = writeln!(
                    output,
                    "- {} [{}] {}",
                    task.task_id,
                    if task.state.is_empty() {
                        "unknown"
                    } else {
                        &task.state
                    },
                    task.objective
                );
                if !task.blockers.is_empty() {
                    let _ = writeln!(output, "  - blockers: {}", task.blockers.join(", "));
                }
                if let Some(phase) = &task.last_checkpoint_phase {
                    let _ = writeln!(output, "  - phase: {}", phase);
                }
                if !task.latest_update.is_empty() {
                    let _ = writeln!(output, "  - update: {}", task.latest_update);
                }
                if let Some(verification) = &task.verification {
                    if let Some(exit_code) = verification.exit_code {
                        let _ = writeln!(
                            output,
                            "  - verify: {} ({}, exit={})",
                            verification.command, verification.status, exit_code
                        );
                    } else {
                        let _ = writeln!(
                            output,
                            "  - verify: {} ({})",
                            verification.command, verification.status
                        );
                    }
                }
                if let Some(close_outcome) = &task.close_outcome {
                    let _ = writeln!(output, "  - close: {}", close_outcome);
                }
            }
            output.push('\n');
        }

        // Key Decisions
        if !self.key_decisions.is_empty() {
            output.push_str("## Key Decisions\n");
            for decision in &self.key_decisions {
                let _ = writeln!(
                    output,
                    "- **{}**: {}",
                    decision.decision, decision.rationale
                );
            }
            output.push('\n');
        }

        // Error History
        if !self.error_history.is_empty() {
            output.push_str("## Error History\n");
            for error in &self.error_history {
                if let Some(resolution) = &error.resolution {
                    let _ = writeln!(output, "- {} (resolved: {})", error.error, resolution);
                } else {
                    let _ = writeln!(output, "- {} (unresolved)", error.error);
                }
            }
            output.push('\n');
        }

        output.trim_end().to_string()
    }

    /// Format the summary as a dense checkpoint string for prompt reuse.
    ///
    /// This keeps the structure explicit while cutting markdown overhead and
    /// adding per-section counts to make truncation or omission easier to spot.
    pub fn to_checkpoint_string(&self) -> String {
        let mut output = String::from("CHECKPOINT v2\n");

        if !self.user_intent.is_empty() {
            let _ = writeln!(output, "goal: {}", self.user_intent.trim());
        }

        let _ = writeln!(output, "done[{}]:", self.completed_tasks.len());
        for task in &self.completed_tasks {
            if task.details.trim().is_empty() {
                let _ = writeln!(output, "- {} | {}", task.result.trim(), task.task.trim());
            } else {
                let _ = writeln!(
                    output,
                    "- {} | {} | {}",
                    task.result.trim(),
                    task.task.trim(),
                    task.details.trim()
                );
            }
        }

        let _ = writeln!(output, "pending[{}]:", self.pending_tasks.len());
        for task in &self.pending_tasks {
            let _ = writeln!(output, "- {}", task.trim());
        }

        let _ = writeln!(output, "files_examined[{}]:", self.files_examined.len());
        for file in &self.files_examined {
            if file.key_insights.is_empty() {
                let _ = writeln!(output, "- {} | {}", file.path.trim(), file.purpose.trim());
            } else {
                let _ = writeln!(
                    output,
                    "- {} | {} | {}",
                    file.path.trim(),
                    file.purpose.trim(),
                    file.key_insights.join(" ; ")
                );
            }
        }

        let _ = writeln!(output, "files_modified[{}]:", self.files_modified.len());
        for change in &self.files_modified {
            let _ = writeln!(
                output,
                "- {} | {} | {}",
                change.path.trim(),
                change.change_type,
                change.summary.trim()
            );
        }

        if !self.current_state.is_empty() {
            let _ = writeln!(output, "state: {}", self.current_state.trim());
        }

        if let Some(orchestration) = &self.orchestration {
            if !orchestration.is_empty() {
                let mut parts = Vec::new();
                if !orchestration.phase.trim().is_empty() {
                    parts.push(orchestration.phase.trim().to_string());
                }
                if !orchestration.objective.trim().is_empty() {
                    parts.push(orchestration.objective.trim().to_string());
                }
                if let Some(next_action) = &orchestration.next_action {
                    let next_action = next_action.trim();
                    if !next_action.is_empty() {
                        parts.push(format!("next={next_action}"));
                    }
                }
                if !parts.is_empty() {
                    let _ = writeln!(output, "orchestration: {}", parts.join(" | "));
                }
                let _ = writeln!(
                    output,
                    "orchestration_blockers[{}]:",
                    orchestration.blockers.len()
                );
                for blocker in &orchestration.blockers {
                    let _ = writeln!(output, "- {}", blocker.trim());
                }
                let _ = writeln!(
                    output,
                    "orchestration_recent[{}]:",
                    orchestration.recent_actions.len()
                );
                for action in &orchestration.recent_actions {
                    let _ = writeln!(output, "- {}", action.trim());
                }
            }
        }

        if !self.runtime_tasks.is_empty() {
            let mut counts = BTreeMap::new();
            for task in &self.runtime_tasks {
                let key = if task.state.trim().is_empty() {
                    "unknown".to_string()
                } else {
                    task.state.trim().to_string()
                };
                *counts.entry(key).or_insert(0usize) += 1;
            }
            let _ = writeln!(output, "task_counts[{}]:", counts.len());
            for (state, count) in counts {
                let _ = writeln!(output, "- {}={}", state, count);
            }

            let mut tasks = self.runtime_tasks.clone();
            tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
            let _ = writeln!(output, "tasks[{}]:", tasks.len());
            for task in &tasks {
                let mut parts = vec![task.task_id.trim().to_string()];
                if !task.state.trim().is_empty() {
                    parts.push(task.state.trim().to_string());
                }
                if !task.objective.trim().is_empty() {
                    parts.push(task.objective.trim().to_string());
                }
                if !task.blockers.is_empty() {
                    parts.push(format!("blockers={}", task.blockers.join(" ; ")));
                }
                if let Some(phase) = &task.last_checkpoint_phase {
                    let phase = phase.trim();
                    if !phase.is_empty() {
                        parts.push(format!("phase={phase}"));
                    }
                }
                if !task.latest_update.trim().is_empty() {
                    parts.push(format!("update={}", task.latest_update.trim()));
                }
                if let Some(verification) = &task.verification {
                    let mut verify = format!(
                        "verify={}: {}",
                        verification.status.trim(),
                        verification.command.trim()
                    );
                    if let Some(exit_code) = verification.exit_code {
                        verify.push_str(&format!(" (exit={exit_code})"));
                    }
                    parts.push(verify);
                }
                if let Some(close_outcome) = &task.close_outcome {
                    let close_outcome = close_outcome.trim();
                    if !close_outcome.is_empty() {
                        parts.push(format!("close={close_outcome}"));
                    }
                }
                let _ = writeln!(output, "- {}", parts.join(" | "));
            }
        }

        let _ = writeln!(output, "decisions[{}]:", self.key_decisions.len());
        for decision in &self.key_decisions {
            let _ = writeln!(
                output,
                "- {} | {}",
                decision.decision.trim(),
                decision.rationale.trim()
            );
        }

        let _ = writeln!(output, "errors[{}]:", self.error_history.len());
        for error in &self.error_history {
            match &error.resolution {
                Some(resolution) if !resolution.trim().is_empty() => {
                    let _ = writeln!(output, "- {} | {}", error.error.trim(), resolution.trim());
                }
                _ => {
                    let _ = writeln!(output, "- {}", error.error.trim());
                }
            }
        }

        output.trim_end().to_string()
    }

    /// Check if the summary is empty (contains no meaningful information).
    pub fn is_empty(&self) -> bool {
        self.user_intent.is_empty()
            && self.completed_tasks.is_empty()
            && self.pending_tasks.is_empty()
            && self.files_examined.is_empty()
            && self.files_modified.is_empty()
            && self.current_state.is_empty()
            && self
                .orchestration
                .as_ref()
                .is_none_or(OrchestrationSummary::is_empty)
            && self.runtime_tasks.is_empty()
            && self.key_decisions.is_empty()
            && self.error_history.is_empty()
    }
}

/// Task complexity classification for dynamic threshold calculation.
///
/// Used to adjust compaction aggressiveness based on perceived task difficulty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum TaskComplexity {
    /// Simple task: single file change, straightforward fix.
    Low,
    /// Medium task: multi-file changes, some coordination needed.
    #[default]
    Medium,
    /// Complex task: significant refactoring, architectural changes.
    High,
}

impl TaskComplexity {
    /// Convert complexity to a numeric value for calculations.
    pub const fn as_f32(self) -> f32 {
        match self {
            Self::Low => 0.0,
            Self::Medium => 0.5,
            Self::High => 1.0,
        }
    }

    /// Create complexity from the number of files involved.
    pub const fn from_file_count(count: usize) -> Self {
        if count <= 1 {
            Self::Low
        } else if count <= 5 {
            Self::Medium
        } else {
            Self::High
        }
    }

    /// Create complexity from error history length.
    pub const fn from_error_count(count: usize) -> Self {
        if count == 0 {
            Self::Low
        } else if count <= 3 {
            Self::Medium
        } else {
            Self::High
        }
    }
}

/// Calculate dynamic context threshold based on task state.
///
/// This function implements a feedback-driven approach where:
/// - Higher threshold for complex tasks (more context needed)
/// - Higher threshold when errors are occurring (need context to debug)
/// - Base threshold is 70% of context window
/// - Maximum threshold is 90% (always leave room)
///
/// # Arguments
/// * `task_complexity` - How complex the current task is
/// * `error_rate` - Recent error rate (0.0 to 1.0)
///
/// # Returns
/// A percentage (0.0 to 1.0) of the context window to use as threshold
///
/// # Example
///
/// ```
/// use pi::context::summary::{calculate_threshold, TaskComplexity};
///
/// // Simple task, no errors: 70% threshold
/// let threshold = calculate_threshold(TaskComplexity::Low, 0.0);
/// assert_eq!(threshold, 0.7);
///
/// // Complex task, high error rate: 90% threshold (capped)
/// let threshold = calculate_threshold(TaskComplexity::High, 0.8);
/// assert_eq!(threshold, 0.9);
/// ```
pub fn calculate_threshold(task_complexity: TaskComplexity, error_rate: f32) -> f32 {
    // Base threshold: 70% of context window
    let base = 0.7;

    // Bonus for task complexity (up to 10%)
    let complexity_bonus = task_complexity.as_f32() * 0.1;

    // Bonus for error rate (up to 15%) - clamp to [0, 1] to handle negative values
    let error_bonus = error_rate.clamp(0.0, 1.0) * 0.15;

    // Calculate final threshold, capped at 90%
    (base + complexity_bonus + error_bonus).min(0.9).max(0.5)
}

/// Convert threshold percentage to a token count.
///
/// # Arguments
/// * `threshold` - Threshold percentage (0.0 to 1.0)
/// * `context_window_size` - Total context window in tokens
///
/// # Returns
/// Number of tokens to use as the threshold
pub fn threshold_to_tokens(threshold: f32, context_window_size: usize) -> usize {
    ((context_window_size as f32) * threshold.max(0.0).min(1.0)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_type_display() {
        assert_eq!(ChangeType::Created.to_string(), "created");
        assert_eq!(ChangeType::Modified.to_string(), "modified");
        assert_eq!(ChangeType::Deleted.to_string(), "deleted");
    }

    #[test]
    fn change_type_serde_roundtrip() {
        let original = ChangeType::Modified;
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, "\"modified\"");
        let parsed: ChangeType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn file_summary_serde() {
        let summary = FileSummary {
            path: "/src/main.rs".to_string(),
            purpose: "Entry point".to_string(),
            key_insights: vec!["Uses tokio runtime".to_string()],
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: FileSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, summary.path);
        assert_eq!(parsed.purpose, summary.purpose);
        assert_eq!(parsed.key_insights, summary.key_insights);
    }

    #[test]
    fn file_change_serde() {
        let change = FileChange {
            path: "/src/lib.rs".to_string(),
            change_type: ChangeType::Modified,
            summary: "Added new function".to_string(),
        };
        let json = serde_json::to_string(&change).unwrap();
        assert!(json.contains("\"changeType\":\"modified\""));
        let parsed: FileChange = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, change.path);
        assert_eq!(parsed.change_type, change.change_type);
    }

    #[test]
    fn task_outcome_serde() {
        let outcome = TaskOutcome {
            task: "Fix bug".to_string(),
            result: "success".to_string(),
            details: "Fixed null pointer".to_string(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let parsed: TaskOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task, outcome.task);
        assert_eq!(parsed.result, outcome.result);
        assert_eq!(parsed.details, outcome.details);
    }

    #[test]
    fn task_outcome_skips_empty_details() {
        let outcome = TaskOutcome {
            task: "Fix bug".to_string(),
            result: "success".to_string(),
            details: String::new(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(!json.contains("\"details\""));
    }

    #[test]
    fn decision_serde() {
        let decision = Decision {
            decision: "Use PostgreSQL".to_string(),
            rationale: "Better for complex queries".to_string(),
        };
        let json = serde_json::to_string(&decision).unwrap();
        let parsed: Decision = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.decision, decision.decision);
        assert_eq!(parsed.rationale, decision.rationale);
    }

    #[test]
    fn error_record_serde() {
        let record = ErrorRecord {
            error: "Connection refused".to_string(),
            resolution: Some("Increased timeout".to_string()),
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: ErrorRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.error, record.error);
        assert_eq!(parsed.resolution, record.resolution);
    }

    #[test]
    fn error_record_without_resolution() {
        let record = ErrorRecord {
            error: "Unknown error".to_string(),
            resolution: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("\"resolution\""));
        let parsed: ErrorRecord = serde_json::from_str(&json).unwrap();
        assert!(parsed.resolution.is_none());
    }

    #[test]
    fn compaction_summary_default() {
        let summary = CompactionSummary::default();
        assert!(summary.is_empty());
        assert!(summary.user_intent.is_empty());
        assert!(summary.completed_tasks.is_empty());
        assert!(summary.pending_tasks.is_empty());
    }

    #[test]
    fn compaction_summary_is_empty() {
        let mut summary = CompactionSummary {
            user_intent: "Goal".to_string(),
            ..Default::default()
        };
        assert!(!summary.is_empty());

        summary.user_intent = String::new();
        assert!(summary.is_empty());
    }

    #[test]
    fn to_context_string_empty() {
        let summary = CompactionSummary::default();
        let context = summary.to_context_string();
        assert!(context.is_empty());
    }

    #[test]
    fn to_context_string_user_intent() {
        let summary = CompactionSummary {
            user_intent: "Fix the bug".to_string(),
            ..Default::default()
        };
        let context = summary.to_context_string();
        assert!(context.contains("## User Intent"));
        assert!(context.contains("Fix the bug"));
    }

    #[test]
    fn to_context_string_completed_tasks() {
        let summary = CompactionSummary {
            completed_tasks: vec![
                TaskOutcome {
                    task: "Task 1".to_string(),
                    result: "success".to_string(),
                    details: String::new(),
                },
                TaskOutcome {
                    task: "Task 2".to_string(),
                    result: "partial".to_string(),
                    details: "Almost done".to_string(),
                },
                TaskOutcome {
                    task: "Task 3".to_string(),
                    result: "failed".to_string(),
                    details: "Error occurred".to_string(),
                },
            ],
            ..Default::default()
        };
        let context = summary.to_context_string();
        assert!(context.contains("## Completed Tasks"));
        assert!(context.contains("[x] Task 1 (success)"));
        assert!(context.contains("[~] Task 2 (partial)"));
        assert!(context.contains("[!] Task 3 (failed)"));
        assert!(context.contains("Almost done"));
    }

    #[test]
    fn to_context_string_pending_tasks() {
        let summary = CompactionSummary {
            pending_tasks: vec!["Write tests".to_string(), "Review code".to_string()],
            ..Default::default()
        };
        let context = summary.to_context_string();
        assert!(context.contains("## Pending Tasks"));
        assert!(context.contains("- [ ] Write tests"));
        assert!(context.contains("- [ ] Review code"));
    }

    #[test]
    fn to_context_string_files_examined() {
        let summary = CompactionSummary {
            files_examined: vec![FileSummary {
                path: "/src/lib.rs".to_string(),
                purpose: "Main library".to_string(),
                key_insights: vec!["Uses serde".to_string(), "Async enabled".to_string()],
            }],
            ..Default::default()
        };
        let context = summary.to_context_string();
        assert!(context.contains("## Files Examined"));
        assert!(context.contains("`/src/lib.rs`: Main library"));
        assert!(context.contains("- Uses serde"));
        assert!(context.contains("- Async enabled"));
    }

    #[test]
    fn to_context_string_files_modified() {
        let summary = CompactionSummary {
            files_modified: vec![
                FileChange {
                    path: "/src/new.rs".to_string(),
                    change_type: ChangeType::Created,
                    summary: "New module".to_string(),
                },
                FileChange {
                    path: "/src/old.rs".to_string(),
                    change_type: ChangeType::Deleted,
                    summary: "Removed deprecated code".to_string(),
                },
            ],
            ..Default::default()
        };
        let context = summary.to_context_string();
        assert!(context.contains("## Files Modified"));
        assert!(context.contains("`/src/new.rs` (created): New module"));
        assert!(context.contains("`/src/old.rs` (deleted): Removed deprecated code"));
    }

    #[test]
    fn to_context_string_current_state() {
        let summary = CompactionSummary {
            current_state: "Waiting for CI to pass".to_string(),
            ..Default::default()
        };
        let context = summary.to_context_string();
        assert!(context.contains("## Current State"));
        assert!(context.contains("Waiting for CI to pass"));
    }

    #[test]
    fn to_context_string_key_decisions() {
        let summary = CompactionSummary {
            key_decisions: vec![Decision {
                decision: "Use Redis".to_string(),
                rationale: "Faster caching".to_string(),
            }],
            ..Default::default()
        };
        let context = summary.to_context_string();
        assert!(context.contains("## Key Decisions"));
        assert!(context.contains("**Use Redis**: Faster caching"));
    }

    #[test]
    fn to_context_string_error_history() {
        let summary = CompactionSummary {
            error_history: vec![
                ErrorRecord {
                    error: "Build failed".to_string(),
                    resolution: Some("Fixed typo".to_string()),
                },
                ErrorRecord {
                    error: "Test failed".to_string(),
                    resolution: None,
                },
            ],
            ..Default::default()
        };
        let context = summary.to_context_string();
        assert!(context.contains("## Error History"));
        assert!(context.contains("Build failed (resolved: Fixed typo)"));
        assert!(context.contains("Test failed (unresolved)"));
    }

    #[test]
    fn to_context_string_full() {
        let summary = CompactionSummary {
            user_intent: "Implement feature X".to_string(),
            completed_tasks: vec![TaskOutcome {
                task: "Research".to_string(),
                result: "success".to_string(),
                details: String::new(),
            }],
            pending_tasks: vec!["Code it".to_string()],
            files_examined: vec![FileSummary {
                path: "/docs/api.md".to_string(),
                purpose: "API docs".to_string(),
                key_insights: vec![],
            }],
            files_modified: vec![FileChange {
                path: "/src/x.rs".to_string(),
                change_type: ChangeType::Created,
                summary: "Feature X impl".to_string(),
            }],
            current_state: "50% complete".to_string(),
            key_decisions: vec![Decision {
                decision: "Use async".to_string(),
                rationale: "Performance".to_string(),
            }],
            error_history: vec![ErrorRecord {
                error: "Type error".to_string(),
                resolution: Some("Added cast".to_string()),
            }],
            ..Default::default()
        };
        let context = summary.to_context_string();

        // Check all sections are present
        assert!(context.contains("## User Intent"));
        assert!(context.contains("## Completed Tasks"));
        assert!(context.contains("## Pending Tasks"));
        assert!(context.contains("## Files Examined"));
        assert!(context.contains("## Files Modified"));
        assert!(context.contains("## Current State"));
        assert!(context.contains("## Key Decisions"));
        assert!(context.contains("## Error History"));

        // Check content
        assert!(context.contains("Implement feature X"));
        assert!(context.contains("[x] Research (success)"));
        assert!(context.contains("- [ ] Code it"));
        assert!(context.contains("`/docs/api.md`: API docs"));
        assert!(context.contains("`/src/x.rs` (created): Feature X impl"));
        assert!(context.contains("50% complete"));
        assert!(context.contains("**Use async**: Performance"));
        assert!(context.contains("Type error (resolved: Added cast)"));
    }

    #[test]
    fn to_checkpoint_string_full() {
        let summary = CompactionSummary {
            user_intent: "Implement feature X".to_string(),
            completed_tasks: vec![TaskOutcome {
                task: "Research".to_string(),
                result: "success".to_string(),
                details: "Captured edge cases".to_string(),
            }],
            pending_tasks: vec!["Code it".to_string()],
            files_examined: vec![FileSummary {
                path: "/docs/api.md".to_string(),
                purpose: "API docs".to_string(),
                key_insights: vec!["Explains auth flow".to_string()],
            }],
            files_modified: vec![FileChange {
                path: "/src/x.rs".to_string(),
                change_type: ChangeType::Created,
                summary: "Feature X impl".to_string(),
            }],
            current_state: "50% complete".to_string(),
            key_decisions: vec![Decision {
                decision: "Use async".to_string(),
                rationale: "Performance".to_string(),
            }],
            error_history: vec![ErrorRecord {
                error: "Type error".to_string(),
                resolution: Some("Added cast".to_string()),
            }],
            ..Default::default()
        };

        let checkpoint = summary.to_checkpoint_string();

        assert!(checkpoint.contains("CHECKPOINT v2"));
        assert!(checkpoint.contains("done[1]:"));
        assert!(checkpoint.contains("pending[1]:"));
        assert!(checkpoint.contains("files_examined[1]:"));
        assert!(checkpoint.contains("files_modified[1]:"));
        assert!(checkpoint.contains("decisions[1]:"));
        assert!(checkpoint.contains("errors[1]:"));
        assert!(checkpoint.contains("- success | Research | Captured edge cases"));
        assert!(checkpoint.contains("- /docs/api.md | API docs | Explains auth flow"));
        assert!(checkpoint.contains("- /src/x.rs | created | Feature X impl"));
        assert!(checkpoint.contains("state: 50% complete"));
    }

    #[test]
    fn compaction_summary_serde_roundtrip() {
        let original = CompactionSummary {
            user_intent: "Goal".to_string(),
            completed_tasks: vec![TaskOutcome {
                task: "Done".to_string(),
                result: "success".to_string(),
                details: "Details here".to_string(),
            }],
            pending_tasks: vec!["Todo".to_string()],
            files_examined: vec![FileSummary {
                path: "/path".to_string(),
                purpose: "Purpose".to_string(),
                key_insights: vec!["Insight".to_string()],
            }],
            files_modified: vec![FileChange {
                path: "/mod".to_string(),
                change_type: ChangeType::Modified,
                summary: "Changed".to_string(),
            }],
            current_state: "In progress".to_string(),
            key_decisions: vec![Decision {
                decision: "Decided".to_string(),
                rationale: "Because".to_string(),
            }],
            error_history: vec![ErrorRecord {
                error: "Oops".to_string(),
                resolution: Some("Fixed".to_string()),
            }],
            ..Default::default()
        };

        let json = serde_json::to_string(&original).unwrap();
        let parsed: CompactionSummary = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.user_intent, original.user_intent);
        assert_eq!(parsed.completed_tasks.len(), 1);
        assert_eq!(parsed.completed_tasks[0].task, "Done");
        assert_eq!(parsed.pending_tasks, original.pending_tasks);
        assert_eq!(parsed.files_examined.len(), 1);
        assert_eq!(parsed.files_modified.len(), 1);
        assert_eq!(parsed.current_state, original.current_state);
        assert_eq!(parsed.key_decisions.len(), 1);
        assert_eq!(parsed.error_history.len(), 1);
    }

    #[test]
    fn compaction_summary_skips_empty_fields_in_json() {
        let summary = CompactionSummary {
            user_intent: "Goal".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&summary).unwrap();

        // userIntent should be present
        assert!(json.contains("\"userIntent\":\"Goal\""));

        // Empty collections and strings should be skipped
        assert!(!json.contains("\"completedTasks\""));
        assert!(!json.contains("\"pendingTasks\""));
        assert!(!json.contains("\"filesExamined\""));
        assert!(!json.contains("\"filesModified\""));
        assert!(!json.contains("\"currentState\""));
        assert!(!json.contains("\"orchestration\""));
        assert!(!json.contains("\"runtimeTasks\""));
        assert!(!json.contains("\"keyDecisions\""));
        assert!(!json.contains("\"errorHistory\""));
    }

    #[test]
    fn to_checkpoint_string_includes_orchestration_and_runtime_tasks() {
        let summary = CompactionSummary {
            orchestration: Some(OrchestrationSummary {
                objective: "Ship token-efficient compaction".to_string(),
                phase: "dispatching".to_string(),
                blockers: vec!["task-tests waiting on task-compaction".to_string()],
                recent_actions: vec!["planned compaction".to_string()],
                next_action: Some("dispatch ready tasks".to_string()),
            }),
            runtime_tasks: vec![
                RuntimeTaskSummary {
                    task_id: "task-compaction".to_string(),
                    state: "executing".to_string(),
                    objective: "Implement structured checkpoints".to_string(),
                    blockers: Vec::new(),
                    latest_update: String::new(),
                    last_checkpoint_phase: None,
                    verification: Some(VerificationSummary {
                        command: "cargo test compaction".to_string(),
                        status: "passed".to_string(),
                        exit_code: Some(0),
                    }),
                    close_outcome: None,
                },
                RuntimeTaskSummary {
                    task_id: "task-tests".to_string(),
                    state: "blocked".to_string(),
                    objective: "Update compaction tests".to_string(),
                    blockers: vec!["task-compaction".to_string()],
                    latest_update: String::new(),
                    last_checkpoint_phase: None,
                    verification: None,
                    close_outcome: None,
                },
            ],
            ..Default::default()
        };

        let checkpoint = summary.to_checkpoint_string();

        assert!(checkpoint.contains(
            "orchestration: dispatching | Ship token-efficient compaction | next=dispatch ready tasks"
        ));
        assert!(checkpoint.contains("orchestration_blockers[1]:"));
        assert!(checkpoint.contains("orchestration_recent[1]:"));
        assert!(checkpoint.contains("task_counts[2]:"));
        assert!(checkpoint.contains("- blocked=1"));
        assert!(checkpoint.contains("- executing=1"));
        assert!(checkpoint.contains(
            "- task-compaction | executing | Implement structured checkpoints | verify=passed: cargo test compaction (exit=0)"
        ));
        assert!(checkpoint.contains(
            "- task-tests | blocked | Update compaction tests | blockers=task-compaction"
        ));
    }

    #[test]
    fn task_complexity_default() {
        assert_eq!(TaskComplexity::default(), TaskComplexity::Medium);
    }

    #[test]
    fn task_complexity_as_f32() {
        assert_eq!(TaskComplexity::Low.as_f32(), 0.0);
        assert_eq!(TaskComplexity::Medium.as_f32(), 0.5);
        assert_eq!(TaskComplexity::High.as_f32(), 1.0);
    }

    #[test]
    fn task_complexity_from_file_count() {
        assert_eq!(TaskComplexity::from_file_count(0), TaskComplexity::Low);
        assert_eq!(TaskComplexity::from_file_count(1), TaskComplexity::Low);
        assert_eq!(TaskComplexity::from_file_count(3), TaskComplexity::Medium);
        assert_eq!(TaskComplexity::from_file_count(5), TaskComplexity::Medium);
        assert_eq!(TaskComplexity::from_file_count(6), TaskComplexity::High);
        assert_eq!(TaskComplexity::from_file_count(100), TaskComplexity::High);
    }

    #[test]
    fn task_complexity_from_error_count() {
        assert_eq!(TaskComplexity::from_error_count(0), TaskComplexity::Low);
        assert_eq!(TaskComplexity::from_error_count(1), TaskComplexity::Medium);
        assert_eq!(TaskComplexity::from_error_count(3), TaskComplexity::Medium);
        assert_eq!(TaskComplexity::from_error_count(4), TaskComplexity::High);
        assert_eq!(TaskComplexity::from_error_count(10), TaskComplexity::High);
    }

    #[test]
    fn calculate_threshold_base() {
        // Low complexity, no errors: base 70%
        let threshold = calculate_threshold(TaskComplexity::Low, 0.0);
        assert!((threshold - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn calculate_threshold_with_complexity() {
        // High complexity: 70% + 10% = 80%
        let threshold = calculate_threshold(TaskComplexity::High, 0.0);
        assert!((threshold - 0.8).abs() < f32::EPSILON);

        // Medium complexity: 70% + 5% = 75%
        let threshold = calculate_threshold(TaskComplexity::Medium, 0.0);
        assert!((threshold - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn calculate_threshold_with_errors() {
        // High error rate: 70% + 15% = 85%
        let threshold = calculate_threshold(TaskComplexity::Low, 1.0);
        assert!((threshold - 0.85).abs() < f32::EPSILON);

        // Medium error rate (0.5): 70% + 7.5% = 77.5%
        let threshold = calculate_threshold(TaskComplexity::Low, 0.5);
        assert!((threshold - 0.775).abs() < 0.001);
    }

    #[test]
    fn calculate_threshold_combined() {
        // High complexity + high errors: 70% + 10% + 15% = 95% -> capped at 90%
        let threshold = calculate_threshold(TaskComplexity::High, 1.0);
        assert!((threshold - 0.9).abs() < f32::EPSILON);

        // Medium complexity + medium errors: 70% + 5% + 7.5% = 82.5%
        let threshold = calculate_threshold(TaskComplexity::Medium, 0.5);
        assert!((threshold - 0.825).abs() < 0.001);
    }

    #[test]
    fn calculate_threshold_minimum() {
        // With negative error rate (clamped to 0) and low complexity, threshold is base
        let threshold = calculate_threshold(TaskComplexity::Low, -1.0);
        assert!(
            (threshold - 0.7).abs() < 0.001,
            "threshold should be ~0.7, got {threshold}"
        );
    }

    #[test]
    fn calculate_threshold_maximum() {
        // Maximum threshold is 90%
        let threshold = calculate_threshold(TaskComplexity::High, 2.0);
        assert!(
            (threshold - 0.9).abs() < 0.001,
            "threshold should be ~0.9, got {threshold}"
        );
    }

    #[test]
    fn threshold_to_tokens_basic() {
        let tokens = threshold_to_tokens(0.7, 10000);
        assert_eq!(tokens, 7000);
    }

    #[test]
    fn threshold_to_tokens_clamped() {
        // Threshold above 1.0 is clamped
        let tokens = threshold_to_tokens(1.5, 10000);
        assert_eq!(tokens, 10000);

        // Threshold below 0.0 is clamped
        let tokens = threshold_to_tokens(-0.5, 10000);
        assert_eq!(tokens, 0);
    }

    #[test]
    fn task_complexity_serde() {
        let original = TaskComplexity::High;
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, "\"high\"");

        let parsed: TaskComplexity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn task_complexity_ord() {
        assert!(TaskComplexity::Low <= TaskComplexity::Medium);
        assert!(TaskComplexity::Medium <= TaskComplexity::High);
    }

    #[test]
    fn task_complexity_partial_order() {
        assert!(TaskComplexity::Low < TaskComplexity::High);
        assert!(TaskComplexity::Medium >= TaskComplexity::Low);
    }
}
