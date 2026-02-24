//! Stuck/drift detection for agent behavior analysis.
//!
//! This module provides detection of stuck patterns in agent execution,
//! such as repeating actions, repeated errors, lack of progress, or
//! excessive text output without tool use (monologuing).
//!
//! # Example
//!
//! ```
//! use pi::reliability::stuck::{StuckDetector, StuckPattern};
//! use pi::events::Action;
//!
//! let mut detector = StuckDetector::new();
//!
//! // Record actions and check for stuck patterns
//! detector.record_action(&Action::read("/tmp/file.txt"));
//! detector.record_action(&Action::read("/tmp/file.txt"));
//! detector.record_action(&Action::read("/tmp/file.txt"));
//!
//! if let Some(pattern) = detector.check() {
//!     match pattern {
//!         StuckPattern::RepeatedAction { action, count } => {
//!             println!("Agent stuck repeating action: {} {} times", action, count);
//!         }
//!         _ => println!("Agent stuck: {:?}", pattern),
//!     }
//! }
//! ```
#![allow(clippy::missing_const_for_fn)]

use crate::events::Action;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::VecDeque;

/// Patterns indicating the agent is stuck.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StuckPattern {
    /// Same action repeated multiple times.
    RepeatedAction {
        /// String representation of the action.
        action: String,
        /// Number of times it was repeated.
        count: usize,
    },

    /// Same error occurring multiple times.
    RepeatedError {
        /// The error message.
        error: String,
        /// Number of times it was repeated.
        count: usize,
    },

    /// No measurable progress for N rounds.
    NoProgress {
        /// Number of rounds without progress.
        rounds: usize,
    },

    /// Only text output for N rounds (no tool use).
    Monologue {
        /// Number of consecutive text-only rounds.
        text_only_rounds: usize,
    },
}

impl StuckPattern {
    /// Get a human-readable description of the pattern.
    pub fn description(&self) -> String {
        match self {
            Self::RepeatedAction { action, count } => {
                format!("Action repeated {count} times: {action}")
            }
            Self::RepeatedError { error, count } => {
                format!("Error repeated {count} times: {error}")
            }
            Self::NoProgress { rounds } => {
                format!("No progress for {rounds} rounds")
            }
            Self::Monologue { text_only_rounds } => {
                format!("Text-only output for {text_only_rounds} rounds (no tool use)")
            }
        }
    }

    /// Get the severity level of this pattern.
    pub fn severity(&self) -> StuckSeverity {
        match self {
            Self::RepeatedAction { count, .. } | Self::RepeatedError { count, .. } => {
                if *count >= 5 {
                    StuckSeverity::Critical
                } else if *count >= 3 {
                    StuckSeverity::Warning
                } else {
                    StuckSeverity::Info
                }
            }
            Self::NoProgress { rounds } => {
                if *rounds >= 10 {
                    StuckSeverity::Critical
                } else if *rounds >= 5 {
                    StuckSeverity::Warning
                } else {
                    StuckSeverity::Info
                }
            }
            Self::Monologue { text_only_rounds } => {
                if *text_only_rounds >= 5 {
                    StuckSeverity::Warning
                } else {
                    StuckSeverity::Info
                }
            }
        }
    }
}

/// Severity level for stuck patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StuckSeverity {
    /// Informational - pattern detected but not critical.
    Info,
    /// Warning - action may be needed.
    Warning,
    /// Critical - immediate intervention required.
    Critical,
}

/// Detector for stuck/loop patterns in agent behavior.
#[derive(Debug, Clone)]
pub struct StuckDetector {
    recent_actions: VecDeque<String>,
    recent_errors: VecDeque<String>,
    action_counts: HashMap<String, usize>,
    error_counts: HashMap<String, usize>,
    consecutive_text_rounds: usize,
    no_progress_rounds: usize,
    last_progress_action: Option<String>,
    repetition_threshold: usize,
    no_progress_threshold: usize,
    monologue_threshold: usize,
    max_history: usize,
}

impl Default for StuckDetector {
    fn default() -> Self {
        Self {
            recent_actions: VecDeque::with_capacity(10),
            recent_errors: VecDeque::with_capacity(10),
            action_counts: HashMap::new(),
            error_counts: HashMap::new(),
            consecutive_text_rounds: 0,
            no_progress_rounds: 0,
            last_progress_action: None,
            repetition_threshold: 3,
            no_progress_threshold: 5,
            monologue_threshold: 3,
            max_history: 20,
        }
    }
}

impl StuckDetector {
    /// Create a new stuck detector with default thresholds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new stuck detector with custom thresholds.
    pub fn with_thresholds(repetition: usize, no_progress: usize, monologue: usize) -> Self {
        Self {
            repetition_threshold: repetition,
            no_progress_threshold: no_progress,
            monologue_threshold: monologue,
            ..Self::default()
        }
    }

    /// Set the maximum history size for tracking actions/errors.
    #[must_use]
    pub fn with_max_history(mut self, max_history: usize) -> Self {
        self.max_history = max_history;
        self.recent_actions = VecDeque::with_capacity(max_history);
        self.recent_errors = VecDeque::with_capacity(max_history);
        self
    }

    /// Record an action that was executed.
    pub fn record_action(&mut self, action: &Action) {
        let action_str = Self::action_to_string(action);
        self.record_action_string(action_str);
        self.reset_text_round();
    }

    /// Record an action as a string.
    fn record_action_string(&mut self, action_str: String) {
        // Update count
        *self.action_counts.entry(action_str.clone()).or_insert(0) += 1;

        // Add to history
        self.recent_actions.push_back(action_str.clone());
        if self.recent_actions.len() > self.max_history {
            if let Some(old) = self.recent_actions.pop_front() {
                // Decrement count, remove if zero
                if let Some(count) = self.action_counts.get_mut(&old) {
                    *count -= 1;
                    if *count == 0 {
                        self.action_counts.remove(&old);
                    }
                }
            }
        }

        // Check for progress (action is different from last progress)
        if self
            .last_progress_action
            .as_deref()
            .is_none_or(|last| last != action_str)
        {
            self.last_progress_action = Some(action_str);
            self.no_progress_rounds = 0;
        } else {
            self.no_progress_rounds += 1;
        }
    }

    /// Record an error that occurred.
    pub fn record_error(&mut self, error: &str) {
        let error_str = Self::normalize_error(error);

        // Update count
        *self.error_counts.entry(error_str.clone()).or_insert(0) += 1;

        // Add to history
        self.recent_errors.push_back(error_str);
        if self.recent_errors.len() > self.max_history {
            if let Some(old) = self.recent_errors.pop_front() {
                // Decrement count, remove if zero
                if let Some(count) = self.error_counts.get_mut(&old) {
                    *count -= 1;
                    if *count == 0 {
                        self.error_counts.remove(&old);
                    }
                }
            }
        }
    }

    /// Record a text-only round (no tool use).
    pub fn record_text_round(&mut self) {
        self.consecutive_text_rounds += 1;
    }

    /// Record a tool use round (resets text-only counter).
    pub fn record_tool_round(&mut self) {
        self.reset_text_round();
    }

    /// Reset the consecutive text rounds counter.
    fn reset_text_round(&mut self) {
        self.consecutive_text_rounds = 0;
    }

    /// Check if any stuck pattern is detected.
    pub fn check(&self) -> Option<StuckPattern> {
        // Check for no progress first: this is the strongest signal that work
        // has stalled, even when action repetition is also present.
        if self.no_progress_rounds >= self.no_progress_threshold {
            return Some(StuckPattern::NoProgress {
                rounds: self.no_progress_rounds,
            });
        }

        // Check for repeated actions
        for (action, &count) in &self.action_counts {
            if count >= self.repetition_threshold {
                return Some(StuckPattern::RepeatedAction {
                    action: action.clone(),
                    count,
                });
            }
        }

        // Check for repeated errors
        for (error, &count) in &self.error_counts {
            if count >= self.repetition_threshold {
                return Some(StuckPattern::RepeatedError {
                    error: error.clone(),
                    count,
                });
            }
        }

        // Check for monologue (text-only rounds)
        if self.consecutive_text_rounds >= self.monologue_threshold {
            return Some(StuckPattern::Monologue {
                text_only_rounds: self.consecutive_text_rounds,
            });
        }

        None
    }

    /// Reset all history and counters.
    pub fn reset(&mut self) {
        self.recent_actions.clear();
        self.recent_errors.clear();
        self.action_counts.clear();
        self.error_counts.clear();
        self.consecutive_text_rounds = 0;
        self.no_progress_rounds = 0;
        self.last_progress_action = None;
    }

    /// Check if the agent is currently stuck.
    pub fn is_stuck(&self) -> bool {
        self.check().is_some()
    }

    /// Get the current stuck pattern, if any.
    pub fn pattern(&self) -> Option<StuckPattern> {
        self.check()
    }

    /// Get the count of a specific action.
    pub fn action_count(&self, action: &Action) -> usize {
        let action_str = Self::action_to_string(action);
        self.action_counts.get(&action_str).copied().unwrap_or(0)
    }

    /// Get the count of a specific error.
    pub fn error_count(&self, error: &str) -> usize {
        let error_str = Self::normalize_error(error);
        self.error_counts.get(&error_str).copied().unwrap_or(0)
    }

    /// Get the number of consecutive text-only rounds.
    pub fn text_only_rounds(&self) -> usize {
        self.consecutive_text_rounds
    }

    /// Get the number of rounds without progress.
    pub fn no_progress_rounds(&self) -> usize {
        self.no_progress_rounds
    }

    /// Get the number of distinct actions tracked.
    pub fn distinct_actions(&self) -> usize {
        self.action_counts.len()
    }

    /// Get the number of distinct errors tracked.
    pub fn distinct_errors(&self) -> usize {
        self.error_counts.len()
    }

    /// Convert an action to a normalized string representation.
    fn action_to_string(action: &Action) -> String {
        match action {
            Action::Read(a) => format!("Read({})", a.file_path),
            Action::Write(a) => format!("Write({})", a.file_path),
            Action::Edit(a) => format!("Edit({})", a.file_path),
            Action::Bash(a) => {
                // Normalize bash commands by extracting the first word
                let first_word = a.command.split_whitespace().next().unwrap_or("");
                format!("Bash({first_word})")
            }
            Action::Grep(a) => format!("Grep({})", a.pattern),
            Action::Glob(a) => format!("Glob({})", a.pattern),
            Action::Ls(_) => "Ls".to_string(),
            Action::WebSearch(a) => format!("WebSearch({})", truncate(&a.query, 30)),
            Action::WebFetch(a) => format!("WebFetch({})", a.url),
            Action::Lsp(a) => format!("Lsp({:?})", a.operation),
            Action::Custom { name, .. } => format!("Custom({name})"),
        }
    }

    /// Normalize an error message for comparison.
    fn normalize_error(error: &str) -> String {
        let first_line = error.lines().next().unwrap_or("").trim();
        let stable_prefix = first_line.split(':').next().unwrap_or(first_line).trim();

        // Remove highly variable punctuation/path separators and normalize spacing.
        let normalized = stable_prefix
            .replace(
                |c: char| !c.is_ascii_alphanumeric() && !c.is_whitespace() && c != '_',
                "",
            )
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        // Take first 50 chars
        truncate(&normalized, 50).to_string()
    }
}

/// Truncate a string to a maximum length.
fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len.min(s.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_read_action(path: &str) -> Action {
        Action::Read(crate::events::ReadAction {
            file_path: path.to_string(),
            offset: None,
            limit: None,
        })
    }

    fn create_bash_action(cmd: &str) -> Action {
        Action::Bash(crate::events::BashAction {
            command: cmd.to_string(),
            cwd: None,
            timeout: 120_000,
            env: std::collections::HashMap::new(),
            stream: false,
        })
    }

    #[test]
    fn test_detector_default_creation() {
        let detector = StuckDetector::new();
        assert_eq!(detector.repetition_threshold, 3);
        assert_eq!(detector.no_progress_threshold, 5);
        assert_eq!(detector.monologue_threshold, 3);
        assert!(!detector.is_stuck());
    }

    #[test]
    fn test_detector_custom_thresholds() {
        let detector = StuckDetector::with_thresholds(5, 10, 5);
        assert_eq!(detector.repetition_threshold, 5);
        assert_eq!(detector.no_progress_threshold, 10);
        assert_eq!(detector.monologue_threshold, 5);
    }

    #[test]
    fn test_repeated_action_detection() {
        let mut detector = StuckDetector::with_thresholds(3, 5, 5);
        let action = create_read_action("/tmp/file.txt");

        detector.record_action(&action);
        detector.record_action(&action);
        assert!(!detector.is_stuck());

        detector.record_action(&action);
        assert!(detector.is_stuck());

        if let Some(StuckPattern::RepeatedAction { action, count }) = detector.check() {
            assert!(action.contains("Read"));
            assert_eq!(count, 3);
        } else {
            panic!("Expected RepeatedAction pattern");
        }
    }

    #[test]
    fn test_repeated_error_detection() {
        let mut detector = StuckDetector::with_thresholds(3, 5, 5);

        detector.record_error("File not found: /tmp/file.txt");
        detector.record_error("File not found: /tmp/other.txt");
        detector.record_error("File not found: /tmp/third.txt");

        assert!(detector.is_stuck());

        if let Some(StuckPattern::RepeatedError { error: _, count }) = detector.check() {
            assert_eq!(count, 3);
        } else {
            panic!("Expected RepeatedError pattern");
        }
    }

    #[test]
    fn test_no_progress_detection() {
        let mut detector = StuckDetector::with_thresholds(5, 3, 5);
        let action = create_read_action("/tmp/file.txt");

        // Record the same action multiple times
        for _ in 0..5 {
            detector.record_action(&action);
        }

        // Should trigger no progress after threshold
        assert!(detector.is_stuck());

        if let Some(StuckPattern::NoProgress { rounds }) = detector.check() {
            assert!(rounds >= 3);
        } else {
            panic!("Expected NoProgress pattern");
        }
    }

    #[test]
    fn test_monologue_detection() {
        let mut detector = StuckDetector::with_thresholds(5, 10, 3);

        detector.record_text_round();
        detector.record_text_round();
        assert!(!detector.is_stuck());

        detector.record_text_round();
        assert!(detector.is_stuck());

        if let Some(StuckPattern::Monologue { text_only_rounds }) = detector.check() {
            assert_eq!(text_only_rounds, 3);
        } else {
            panic!("Expected Monologue pattern");
        }
    }

    #[test]
    fn test_tool_round_resets_monologue() {
        let mut detector = StuckDetector::with_thresholds(5, 10, 3);

        detector.record_text_round();
        detector.record_text_round();
        detector.record_tool_round();
        detector.record_text_round();
        detector.record_text_round();

        // Should not be stuck since tool round reset the counter
        assert!(!detector.is_stuck());
    }

    #[test]
    fn test_action_to_string_normalization() {
        let read1 = create_read_action("/tmp/file1.txt");
        let read2 = create_read_action("/tmp/file2.txt");

        let s1 = StuckDetector::action_to_string(&read1);
        let s2 = StuckDetector::action_to_string(&read2);

        // Different paths should produce different strings
        assert_ne!(s1, s2);
        assert!(s1.contains("Read"));
        assert!(s2.contains("Read"));
    }

    #[test]
    fn test_bash_command_normalization() {
        let bash1 = create_bash_action("cargo test --test foo");
        let bash2 = create_bash_action("cargo test --test bar");

        let s1 = StuckDetector::action_to_string(&bash1);
        let s2 = StuckDetector::action_to_string(&bash2);

        // Should both normalize to "Bash(cargo)"
        assert_eq!(s1, "Bash(cargo)");
        assert_eq!(s2, "Bash(cargo)");
    }

    #[test]
    fn test_reset_clears_all_state() {
        let mut detector = StuckDetector::with_thresholds(3, 5, 3);

        detector.record_action(&create_read_action("/tmp/file.txt"));
        detector.record_error("Some error");
        detector.record_text_round();
        detector.record_text_round();
        detector.record_text_round();

        assert!(detector.is_stuck());

        detector.reset();

        assert!(!detector.is_stuck());
        assert_eq!(detector.text_only_rounds(), 0);
        assert_eq!(detector.no_progress_rounds(), 0);
        assert_eq!(detector.distinct_actions(), 0);
        assert_eq!(detector.distinct_errors(), 0);
    }

    #[test]
    fn test_action_count() {
        let mut detector = StuckDetector::new();
        let action = create_read_action("/tmp/file.txt");

        assert_eq!(detector.action_count(&action), 0);

        detector.record_action(&action);
        detector.record_action(&action);

        assert_eq!(detector.action_count(&action), 2);
    }

    #[test]
    fn test_error_count() {
        let mut detector = StuckDetector::new();

        assert_eq!(detector.error_count("File not found"), 0);

        detector.record_error("File not found: /tmp/file.txt");
        detector.record_error("File not found: /tmp/other.txt");

        assert_eq!(detector.error_count("File not found"), 2);
    }

    #[test]
    fn test_stuck_pattern_description() {
        let pattern = StuckPattern::RepeatedAction {
            action: "Read(/tmp/file.txt)".to_string(),
            count: 3,
        };

        let desc = pattern.description();
        assert!(desc.contains("3 times"));
        assert!(desc.contains("Read"));
    }

    #[test]
    fn test_stuck_pattern_severity() {
        // Critical repetition
        let pattern = StuckPattern::RepeatedAction {
            action: "test".to_string(),
            count: 5,
        };
        assert_eq!(pattern.severity(), StuckSeverity::Critical);

        // Warning repetition
        let pattern = StuckPattern::RepeatedAction {
            action: "test".to_string(),
            count: 3,
        };
        assert_eq!(pattern.severity(), StuckSeverity::Warning);

        // Info repetition
        let pattern = StuckPattern::RepeatedAction {
            action: "test".to_string(),
            count: 2,
        };
        assert_eq!(pattern.severity(), StuckSeverity::Info);
    }

    #[test]
    fn test_history_eviction() {
        let mut detector = StuckDetector::new().with_max_history(5);

        // Add more actions than max_history
        for i in 0..10 {
            detector.record_action(&create_read_action(&format!("/tmp/file{i}.txt")));
        }

        // Old actions should be evicted
        assert_eq!(detector.distinct_actions(), 5);
    }

    #[test]
    fn test_different_actions_prevent_no_progress() {
        let mut detector = StuckDetector::with_thresholds(5, 3, 5);

        detector.record_action(&create_read_action("/tmp/file1.txt"));
        detector.record_action(&create_read_action("/tmp/file2.txt"));
        detector.record_action(&create_read_action("/tmp/file3.txt"));

        // Should not trigger no progress since actions are different
        assert!(!detector.is_stuck());
        assert_eq!(detector.no_progress_rounds(), 0);
    }

    #[test]
    fn test_max_history_default() {
        let detector = StuckDetector::new();
        assert_eq!(detector.max_history, 20);
    }
}
