//! RetryAgent with best-of-N selection (SWE-agent approach).
//!
//! For complex tasks, multiple attempts can be made and the best result selected.
//! This implements the SWE-agent approach of running multiple solution attempts
//! and choosing the one with the highest score that passes verification.
//!
//! # Example
//!
//! ```
//! use pi::reliability::retry::{RetryAgent, RetryConfig, Attempt, VerificationOutcome};
//!
//! let config = RetryConfig {
//!     max_attempts: 3,
//!     score_threshold: 0.7,
//!     best_of_n: true,
//! };
//!
//! let mut agent = RetryAgent::new(config);
//!
//! // Record attempts
//! agent.record_attempt(Attempt::new("patch1", VerificationOutcome::Passed, 0.8));
//! agent.record_attempt(Attempt::new("patch2", VerificationOutcome::Passed, 0.9));
//!
//! // Get best result
//! if let Some(best) = agent.final_result() {
//!     println!("Best patch score: {}", best.score);
//! }
//! ```
#![allow(clippy::missing_const_for_fn, clippy::cast_precision_loss)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Outcome of verification for an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationOutcome {
    /// Verification passed successfully.
    Passed,

    /// Verification failed.
    Failed,

    /// Verification was not run.
    #[default]
    NotRun,
}

/// Token usage for an attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    /// Create new token usage from input/output counts.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
        }
    }
}

/// An attempt at solving a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    /// Unique attempt ID.
    pub id: String,

    /// When this attempt was created.
    pub created_at: DateTime<Utc>,

    /// The patch/changes produced by this attempt.
    pub patch: String,

    /// Result of verification (if run).
    pub verification_result: Option<VerificationOutcome>,

    /// Score (0.0 to 1.0).
    pub score: f32,

    /// Test coverage percentage (0.0 to 1.0), if available.
    pub test_coverage: Option<f32>,

    /// Number of files changed.
    pub files_changed: usize,

    /// Lines added/removed.
    pub lines_added: usize,
    pub lines_removed: usize,

    /// Optional notes about this attempt.
    pub notes: Option<String>,

    /// Error message if the attempt failed.
    pub error: Option<String>,

    /// Token usage for this attempt.
    pub tokens_used: Option<TokenUsage>,
}

impl Attempt {
    /// Create a new attempt.
    pub fn new(patch: impl Into<String>, verification: VerificationOutcome, score: f32) -> Self {
        Self {
            id: generate_attempt_id(),
            created_at: Utc::now(),
            patch: patch.into(),
            verification_result: Some(verification),
            score,
            test_coverage: None,
            files_changed: 0,
            lines_added: 0,
            lines_removed: 0,
            notes: None,
            error: None,
            tokens_used: None,
        }
    }

    /// Create an attempt from a patch string.
    pub fn from_patch(patch: impl Into<String>) -> Self {
        Self {
            id: generate_attempt_id(),
            created_at: Utc::now(),
            patch: patch.into(),
            verification_result: None,
            score: 0.0,
            test_coverage: None,
            files_changed: 0,
            lines_added: 0,
            lines_removed: 0,
            notes: None,
            error: None,
            tokens_used: None,
        }
    }

    /// Set verification result.
    #[must_use]
    pub fn with_verification(mut self, result: VerificationOutcome) -> Self {
        self.verification_result = Some(result);
        self
    }

    /// Set score.
    #[must_use]
    pub fn with_score(mut self, score: f32) -> Self {
        self.score = score;
        self
    }

    /// Set test coverage.
    #[must_use]
    pub fn with_coverage(mut self, coverage: f32) -> Self {
        self.test_coverage = Some(coverage);
        self
    }

    /// Set file/line stats.
    #[must_use]
    pub fn with_stats(mut self, files: usize, added: usize, removed: usize) -> Self {
        self.files_changed = files;
        self.lines_added = added;
        self.lines_removed = removed;
        self
    }

    /// Set notes.
    #[must_use]
    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = Some(notes.into());
        self
    }

    /// Set error.
    #[must_use]
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    /// Set token usage.
    #[must_use]
    pub fn with_token_usage(mut self, usage: TokenUsage) -> Self {
        self.tokens_used = Some(usage);
        self
    }

    /// Check if this attempt passed verification.
    pub fn passed(&self) -> bool {
        self.verification_result == Some(VerificationOutcome::Passed)
    }

    /// Check if this attempt has an acceptable score.
    pub fn is_acceptable(&self, threshold: f32) -> bool {
        self.score >= threshold
    }
}

fn generate_attempt_id() -> String {
    format!("attempt-{}", uuid::Uuid::new_v4().simple())
}

/// Configuration for retry behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum number of attempts.
    pub max_attempts: usize,

    /// Minimum score threshold to accept an attempt.
    pub score_threshold: f32,

    /// Whether to use best-of-N selection.
    /// If false, uses the latest attempt that passes verification.
    pub best_of_n: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            score_threshold: 0.7,
            best_of_n: true,
        }
    }
}

impl RetryConfig {
    /// Create a config for single attempt (no retry).
    pub fn single_attempt() -> Self {
        Self {
            max_attempts: 1,
            score_threshold: 0.0,
            best_of_n: false,
        }
    }

    /// Create a config with many attempts for complex tasks.
    pub fn aggressive() -> Self {
        Self {
            max_attempts: 5,
            score_threshold: 0.6,
            best_of_n: true,
        }
    }
}

/// RetryAgent manages multiple attempts at a task.
#[derive(Debug, Clone)]
pub struct RetryAgent {
    config: RetryConfig,
    attempts: Vec<Attempt>,
    token_budget: Option<usize>,
    total_tokens_used: u64,
}

impl RetryAgent {
    /// Create a new RetryAgent with the given configuration.
    pub fn new(config: RetryConfig) -> Self {
        Self {
            config,
            attempts: Vec::new(),
            token_budget: None,
            total_tokens_used: 0,
        }
    }

    /// Create a RetryAgent with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(RetryConfig::default())
    }

    /// Set the token budget for retry attempts.
    #[must_use]
    pub fn with_token_budget(mut self, budget: usize) -> Self {
        self.token_budget = Some(budget);
        self
    }

    /// Check if the token budget has been exhausted.
    pub fn check_token_budget(&self) -> bool {
        self.token_budget.is_none_or(|budget| {
            self.total_tokens_used <= u64::try_from(budget).unwrap_or(u64::MAX)
        })
    }

    /// Record token usage for an attempt.
    pub fn record_token_usage(&mut self, usage: &TokenUsage) {
        self.total_tokens_used = self.total_tokens_used.saturating_add(usage.total_tokens);
    }

    /// Get the total tokens used across all attempts.
    pub fn total_tokens_used(&self) -> u64 {
        self.total_tokens_used
    }

    /// Get the remaining token budget.
    pub fn remaining_token_budget(&self) -> Option<usize> {
        self.token_budget.map(|budget| {
            let used = usize::try_from(self.total_tokens_used).unwrap_or(usize::MAX);
            budget.saturating_sub(used)
        })
    }

    /// Record a new attempt.
    pub fn record_attempt(&mut self, attempt: Attempt) {
        if let Some(usage) = attempt.tokens_used.as_ref() {
            self.record_token_usage(usage);
        }
        self.attempts.push(attempt);
    }

    /// Create and record a new attempt from a patch.
    pub fn new_attempt(&mut self, patch: impl Into<String>) -> &Attempt {
        let attempt = Attempt::from_patch(patch);
        self.attempts.push(attempt);
        self.attempts.last().unwrap()
    }

    /// Check if more attempts can be made.
    pub fn can_retry(&self) -> bool {
        self.attempts.len() < self.config.max_attempts && self.check_token_budget()
    }

    /// Get the number of attempts made.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    /// Get the number of remaining attempts.
    pub fn remaining_attempts(&self) -> usize {
        self.config.max_attempts.saturating_sub(self.attempts.len())
    }

    /// Get all attempts.
    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }

    /// Get attempts that passed verification.
    pub fn passed_attempts(&self) -> Vec<&Attempt> {
        self.attempts.iter().filter(|a| a.passed()).collect()
    }

    /// Get the best attempt (highest score that passed verification).
    pub fn best_attempt(&self) -> Option<&Attempt> {
        self.attempts.iter().filter(|a| a.passed()).max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Get any attempt with an acceptable score (above threshold).
    pub fn acceptable_attempt(&self) -> Option<&Attempt> {
        self.attempts
            .iter()
            .filter(|a| a.is_acceptable(self.config.score_threshold))
            .max_by(|a, b| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// Get the final result based on configuration.
    ///
    /// If `best_of_n` is enabled, returns the best verified attempt.
    /// Otherwise, returns the latest attempt with an acceptable score.
    pub fn final_result(&self) -> Option<&Attempt> {
        if self.config.best_of_n {
            // Prefer verified attempts, then highest score
            self.best_attempt().or_else(|| self.acceptable_attempt())
        } else {
            // Just use the latest
            self.attempts.last()
        }
    }

    /// Score the most recent attempt based on verification and quality metrics.
    pub fn score_latest(&mut self, verification: VerificationOutcome, test_coverage: Option<f32>) {
        if let Some(attempt) = self.attempts.last_mut() {
            attempt.verification_result = Some(verification);
            attempt.test_coverage = test_coverage;
            attempt.score = Self::calculate_score(
                &attempt.patch,
                Some(verification),
                test_coverage,
                attempt.files_changed,
            );
        }
    }

    /// Calculate a score for an attempt.
    ///
    /// Scoring weights:
    /// - Verification status: 50%
    /// - Patch quality: 30%
    /// - Test coverage: 20%
    pub fn calculate_score(
        patch: &str,
        verification: Option<VerificationOutcome>,
        test_coverage: Option<f32>,
        files_changed: usize,
    ) -> f32 {
        let mut score = 0.0;

        // Verification status (50% weight)
        match verification {
            Some(VerificationOutcome::Passed) => score += 0.5,
            Some(VerificationOutcome::Failed) => score += 0.1,
            Some(VerificationOutcome::NotRun) | None => score += 0.25,
        }

        // Patch quality (30% weight)
        let lines_changed = patch.lines().count();
        let has_meaningful_changes = patch.len() > 50 && lines_changed > 3;
        if has_meaningful_changes {
            score += 0.3;
        } else if patch.len() > 10 {
            score += 0.15;
        }

        // Bonus for reasonable scope (not too many files)
        if files_changed > 0 && files_changed <= 5 {
            score += 0.05;
        }

        // Test coverage (15% weight, scaled)
        if let Some(coverage) = test_coverage {
            score += 0.15 * coverage;
        }

        score.min(1.0)
    }

    /// Get the configuration.
    pub fn config(&self) -> &RetryConfig {
        &self.config
    }

    /// Check if any attempt passed.
    pub fn has_passed_attempt(&self) -> bool {
        self.attempts.iter().any(Attempt::passed)
    }

    /// Get statistics about attempts.
    pub fn stats(&self) -> RetryStats {
        let total = self.attempts.len();
        let passed = self.attempts.iter().filter(|a| a.passed()).count();
        let failed = self
            .attempts
            .iter()
            .filter(|a| a.verification_result == Some(VerificationOutcome::Failed))
            .count();

        let avg_score = if total > 0 {
            self.attempts.iter().map(|a| a.score).sum::<f32>() / total as f32
        } else {
            0.0
        };

        let best_score = self.attempts.iter().map(|a| a.score).fold(0.0, f32::max);

        RetryStats {
            total_attempts: total,
            passed,
            failed,
            avg_score,
            best_score,
        }
    }
}

impl Default for RetryAgent {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Get a mutable reference to the last attempt.
impl RetryAgent {
    fn attempts_last_mut(&mut self) -> Option<&mut Attempt> {
        self.attempts.last_mut()
    }
}

/// Statistics about retry attempts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RetryStats {
    /// Total number of attempts.
    pub total_attempts: usize,

    /// Number of attempts that passed verification.
    pub passed: usize,

    /// Number of attempts that failed verification.
    pub failed: usize,

    /// Average score across all attempts.
    pub avg_score: f32,

    /// Best score across all attempts.
    pub best_score: f32,
}

/// Statistics about a single attempt for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptStats {
    pub files_changed: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub duration_ms: u64,
}

/// Submission for review scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSubmission {
    /// The attempt being reviewed.
    pub attempt_id: String,
    /// Trajectory of actions taken.
    pub trajectory: Vec<String>,
    /// Statistics about the attempt.
    pub stats: AttemptStats,
    /// Token usage.
    pub token_usage: Option<TokenUsage>,
}

/// Scorer for review submissions.
#[derive(Debug, Clone)]
pub struct ReviewerScorer {
    model: Option<String>,
    token_budget: Option<usize>,
}

impl ReviewerScorer {
    /// Create a new scorer with default settings.
    pub fn new() -> Self {
        Self {
            model: None,
            token_budget: None,
        }
    }

    /// Set the model to use for scoring.
    #[must_use]
    pub fn with_model(mut self, model: String) -> Self {
        self.model = Some(model);
        self
    }

    /// Set the token budget for scoring.
    #[must_use]
    pub fn with_token_budget(mut self, budget: usize) -> Self {
        self.token_budget = Some(budget);
        self
    }

    /// Score an attempt (0-100).
    /// Returns a score based on verification outcome, test results, and code quality.
    pub fn score(&self, submission: &ReviewSubmission) -> f32 {
        let mut score = 0.0;

        // Base score from test results (0-50 points)
        let total_tests = submission.stats.tests_passed + submission.stats.tests_failed;
        if total_tests > 0 {
            let pass_rate = submission.stats.tests_passed as f32 / total_tests as f32;
            score += pass_rate * 50.0;
        }

        // Code quality score (0-30 points)
        // Prefer smaller, focused changes
        let lines_touched = submission.stats.lines_added + submission.stats.lines_removed;
        if lines_touched > 0 && lines_touched <= 100 {
            score += 30.0;
        } else if lines_touched <= 500 {
            score += 20.0;
        } else {
            score += 10.0;
        }

        // Scope bonus (0-20 points) - prefer fewer files changed
        if (1..=3).contains(&submission.stats.files_changed) {
            score += 20.0;
        } else if (4..=10).contains(&submission.stats.files_changed) {
            score += 10.0;
        }

        score.min(100.0)
    }

    /// Get the model being used for scoring.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Get the token budget.
    pub fn token_budget(&self) -> Option<usize> {
        self.token_budget
    }
}

impl Default for ReviewerScorer {
    fn default() -> Self {
        Self::new()
    }
}

/// Comparison result between two attempts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptComparison {
    /// ID of the winning attempt.
    pub winner_id: String,

    /// Score of the winner.
    pub winner_score: f32,

    /// ID of the losing attempt.
    pub loser_id: String,

    /// Score of the loser.
    pub loser_score: f32,

    /// Explanation of why the winner was chosen.
    pub reason: String,
}

impl RetryAgent {
    /// Compare two attempts and explain why one is better.
    pub fn compare_attempts(a: &Attempt, b: &Attempt) -> AttemptComparison {
        let (winner, loser) = if a.score >= b.score { (a, b) } else { (b, a) };

        let reason = match (winner.verification_result, loser.verification_result) {
            (Some(VerificationOutcome::Passed), Some(VerificationOutcome::Failed)) => {
                "Winner passed verification, loser failed".to_string()
            }
            (Some(VerificationOutcome::Passed), _) => "Winner passed verification".to_string(),
            _ if winner.score > loser.score + 0.1 => {
                format!(
                    "Winner has significantly higher score ({:.2} vs {:.2})",
                    winner.score, loser.score
                )
            }
            _ => {
                format!(
                    "Winner has higher score ({:.2} vs {:.2})",
                    winner.score, loser.score
                )
            }
        };

        AttemptComparison {
            winner_id: winner.id.clone(),
            winner_score: winner.score,
            loser_id: loser.id.clone(),
            loser_score: loser.score,
            reason,
        }
    }

    /// Get the ranking of all attempts by score.
    pub fn rank_attempts(&self) -> Vec<(&Attempt, usize)> {
        let mut indexed: Vec<_> = self.attempts.iter().enumerate().collect();
        indexed.sort_by(|(_, a), (_, b)| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        indexed
            .into_iter()
            .enumerate()
            .map(|(rank, (_, attempt))| (attempt, rank + 1))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attempt_creation() {
        let attempt = Attempt::new("patch", VerificationOutcome::Passed, 0.9);
        assert!(attempt.passed());
        assert!((attempt.score - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn test_attempt_builder() {
        let attempt = Attempt::from_patch("patch")
            .with_verification(VerificationOutcome::Passed)
            .with_score(0.8)
            .with_coverage(0.75)
            .with_stats(3, 100, 50)
            .with_notes("Test attempt");

        assert!(attempt.passed());
        assert!((attempt.score - 0.8).abs() < f32::EPSILON);
        assert_eq!(attempt.test_coverage, Some(0.75));
        assert_eq!(attempt.files_changed, 3);
        assert_eq!(attempt.lines_added, 100);
        assert_eq!(attempt.lines_removed, 50);
    }

    #[test]
    fn test_retry_agent_can_retry() {
        let config = RetryConfig {
            max_attempts: 3,
            score_threshold: 0.7,
            best_of_n: true,
        };
        let mut agent = RetryAgent::new(config);

        assert!(agent.can_retry());
        agent.record_attempt(Attempt::new("p1", VerificationOutcome::Passed, 0.8));
        assert!(agent.can_retry());
        agent.record_attempt(Attempt::new("p2", VerificationOutcome::Passed, 0.9));
        assert!(agent.can_retry());
        agent.record_attempt(Attempt::new("p3", VerificationOutcome::Passed, 0.7));
        assert!(!agent.can_retry());
    }

    #[test]
    fn test_retry_agent_best_attempt() {
        let mut agent = RetryAgent::with_defaults();

        agent.record_attempt(Attempt::new("p1", VerificationOutcome::Passed, 0.7));
        agent.record_attempt(Attempt::new("p2", VerificationOutcome::Passed, 0.9));
        agent.record_attempt(Attempt::new("p3", VerificationOutcome::Passed, 0.8));

        let best = agent.best_attempt();
        assert!(best.is_some());
        assert!((best.unwrap().score - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn test_retry_agent_final_result_best_of_n() {
        let config = RetryConfig {
            max_attempts: 3,
            score_threshold: 0.7,
            best_of_n: true,
        };
        let mut agent = RetryAgent::new(config);

        agent.record_attempt(Attempt::new("p1", VerificationOutcome::Failed, 0.5));
        agent.record_attempt(Attempt::new("p2", VerificationOutcome::Passed, 0.8));
        agent.record_attempt(Attempt::new("p3", VerificationOutcome::Passed, 0.9));

        let result = agent.final_result();
        assert!(result.is_some());
        assert!((result.unwrap().score - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn test_retry_agent_final_result_latest() {
        let config = RetryConfig {
            max_attempts: 3,
            score_threshold: 0.7,
            best_of_n: false, // Use latest
        };
        let mut agent = RetryAgent::new(config);

        agent.record_attempt(Attempt::new("p1", VerificationOutcome::Passed, 0.9));
        agent.record_attempt(Attempt::new("p2", VerificationOutcome::Passed, 0.8));

        let result = agent.final_result();
        assert!(result.is_some());
        // Should be the latest (p2 with score 0.8)
        assert!((result.unwrap().score - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn test_score_calculation_passed() {
        let score = RetryAgent::calculate_score(
            "line1\nline2\nline3\nline4\nline5\nmore text here to make it meaningful",
            Some(VerificationOutcome::Passed),
            Some(1.0),
            2,
        );

        // 0.5 (passed) + 0.3 (meaningful, len > 50) + 0.05 (scope) + 0.15 (coverage) = 1.0
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_score_calculation_failed() {
        let score =
            RetryAgent::calculate_score("small", Some(VerificationOutcome::Failed), None, 1);

        // 0.1 (failed) + 0.05 (scope) + 0 (not meaningful, len <= 10)
        assert!((score - 0.15).abs() < 0.01);
    }

    #[test]
    fn test_retry_stats() {
        let mut agent = RetryAgent::with_defaults();

        agent.record_attempt(Attempt::new("p1", VerificationOutcome::Passed, 0.8));
        agent.record_attempt(Attempt::new("p2", VerificationOutcome::Failed, 0.5));
        agent.record_attempt(Attempt::new("p3", VerificationOutcome::Passed, 0.9));

        let stats = agent.stats();
        assert_eq!(stats.total_attempts, 3);
        assert_eq!(stats.passed, 2);
        assert_eq!(stats.failed, 1);
        assert!((stats.avg_score - 0.733).abs() < 0.01);
        assert!((stats.best_score - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn test_attempt_comparison() {
        let a = Attempt::new("p1", VerificationOutcome::Passed, 0.9);
        let b = Attempt::new("p2", VerificationOutcome::Failed, 0.5);

        let comparison = RetryAgent::compare_attempts(&a, &b);

        assert_eq!(comparison.winner_id, a.id);
        assert_eq!(comparison.loser_id, b.id);
        assert!(comparison.reason.contains("passed verification"));
    }

    #[test]
    fn test_config_defaults() {
        let config = RetryConfig::default();
        assert_eq!(config.max_attempts, 3);
        assert!((config.score_threshold - 0.7).abs() < f32::EPSILON);
        assert!(config.best_of_n);
    }

    #[test]
    fn test_config_single_attempt() {
        let config = RetryConfig::single_attempt();
        assert_eq!(config.max_attempts, 1);
        assert!(!config.best_of_n);
    }

    #[test]
    fn test_config_aggressive() {
        let config = RetryConfig::aggressive();
        assert_eq!(config.max_attempts, 5);
        assert!((config.score_threshold - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn test_rank_attempts() {
        let mut agent = RetryAgent::with_defaults();

        agent.record_attempt(Attempt::new("p1", VerificationOutcome::Passed, 0.7));
        agent.record_attempt(Attempt::new("p2", VerificationOutcome::Passed, 0.9));
        agent.record_attempt(Attempt::new("p3", VerificationOutcome::Passed, 0.8));

        let ranking = agent.rank_attempts();

        assert_eq!(ranking[0].1, 1); // p2 should be rank 1
        assert!((ranking[0].0.score - 0.9).abs() < f32::EPSILON);
    }

    // === Token Budget Tests ===

    #[test]
    fn test_token_usage_creation() {
        let usage = TokenUsage::new(1000, 500);
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.total_tokens, 1500);
    }

    #[test]
    fn test_attempt_with_token_usage() {
        let usage = TokenUsage::new(1000, 500);
        let attempt = Attempt::from_patch("patch").with_token_usage(usage.clone());

        assert_eq!(attempt.tokens_used, Some(usage));
    }

    #[test]
    fn test_retry_agent_token_budget_tracking() {
        let config = RetryConfig::default();
        let mut agent = RetryAgent::new(config).with_token_budget(10000);

        assert_eq!(agent.total_tokens_used(), 0);
        assert!(agent.check_token_budget());

        agent.record_token_usage(&TokenUsage::new(3000, 2000));
        assert_eq!(agent.total_tokens_used(), 5000);
        assert!(agent.check_token_budget());

        agent.record_token_usage(&TokenUsage::new(3000, 2000));
        assert_eq!(agent.total_tokens_used(), 10000);
        // At exactly budget, still allowed
        assert!(agent.check_token_budget());

        agent.record_token_usage(&TokenUsage::new(1, 0));
        assert_eq!(agent.total_tokens_used(), 10001);
        // Over budget, should fail
        assert!(!agent.check_token_budget());
    }

    #[test]
    fn test_retry_agent_can_retry_with_token_budget() {
        let config = RetryConfig {
            max_attempts: 5,
            score_threshold: 0.7,
            best_of_n: true,
        };
        let mut agent = RetryAgent::new(config).with_token_budget(5000);

        // Should be able to retry initially
        assert!(agent.can_retry());

        // Record first attempt with token usage
        agent.record_attempt(
            Attempt::new("p1", VerificationOutcome::Passed, 0.8)
                .with_token_usage(TokenUsage::new(3000, 1000)),
        );
        assert_eq!(agent.total_tokens_used(), 4000);
        assert!(agent.can_retry());

        // Second attempt would exceed budget
        agent.record_token_usage(&TokenUsage::new(2000, 1000));
        assert_eq!(agent.total_tokens_used(), 7000);
        // Even though we have max_attempts left, token budget blocks retry
        assert!(!agent.can_retry());
    }

    #[test]
    fn test_remaining_token_budget() {
        let config = RetryConfig::default();
        let agent = RetryAgent::new(config).with_token_budget(10000);

        assert_eq!(agent.remaining_token_budget(), Some(10000));
    }

    #[test]
    fn test_remaining_token_budget_no_budget_set() {
        let agent = RetryAgent::with_defaults();
        assert_eq!(agent.remaining_token_budget(), None);
        assert!(agent.check_token_budget()); // Always true without budget
    }

    #[test]
    fn test_reviewer_sorer_new() {
        let scorer = ReviewerScorer::new();
        assert!(scorer.model().is_none());
        assert!(scorer.token_budget().is_none());
    }

    #[test]
    fn test_reviewer_sorer_with_model() {
        let scorer = ReviewerScorer::new().with_model("claude-opus-4".to_string());
        assert_eq!(scorer.model(), Some("claude-opus-4"));
    }

    #[test]
    fn test_reviewer_sorer_with_token_budget() {
        let scorer = ReviewerScorer::new().with_token_budget(50000);
        assert_eq!(scorer.token_budget(), Some(50000));
    }

    #[test]
    fn test_reviewer_sorer_score_submission() {
        let scorer = ReviewerScorer::new();
        let submission = ReviewSubmission {
            attempt_id: "test-attempt".to_string(),
            trajectory: vec!["read file".to_string(), "edit file".to_string()],
            stats: AttemptStats {
                files_changed: 2,
                lines_added: 50,
                lines_removed: 10,
                tests_passed: 5,
                tests_failed: 0,
                duration_ms: 1000,
            },
            token_usage: Some(TokenUsage::new(1000, 500)),
        };

        let score = scorer.score(&submission);
        // All tests pass: 50 points
        // Small change: 30 points
        // Few files: 20 points
        // Total: 100
        assert!((score - 100.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_reviewer_sorer_score_partial_fail() {
        let scorer = ReviewerScorer::new();
        let submission = ReviewSubmission {
            attempt_id: "test-attempt".to_string(),
            trajectory: vec!["edit file".to_string()],
            stats: AttemptStats {
                files_changed: 1,
                lines_added: 200,
                lines_removed: 50,
                tests_passed: 3,
                tests_failed: 3,
                duration_ms: 2000,
            },
            token_usage: None,
        };

        let score = scorer.score(&submission);
        // 50% pass rate: 25 points
        // Medium change: 20 points
        // Single file: 20 points
        // Total: 65
        assert!((score - 65.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_reviewer_sorer_default() {
        let scorer = ReviewerScorer::default();
        assert!(scorer.model().is_none());
        assert!(scorer.token_budget().is_none());
    }

    #[test]
    fn test_retry_settings_opt_in_disabled_by_default() {
        use pi::config::RetrySettings;

        let settings = RetrySettings::default();
        assert!(!settings.is_enabled()); // Opt-in: default false
    }

    #[test]
    fn test_retry_settings_explicitly_enabled() {
        use pi::config::RetrySettings;

        let settings = RetrySettings {
            enabled: Some(true),
            ..Default::default()
        };
        assert!(settings.is_enabled());
    }
}
