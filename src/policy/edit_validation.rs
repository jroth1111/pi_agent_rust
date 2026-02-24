//! Multi-strategy edit validation (OpenCode approach).
//!
//! Edits are validated using multiple strategies with fallback:
//! 1. **BlockAnchor** - Find by unique markers (```edit:file.rs)
//! 2. **ContextAware** - Find by surrounding context (50%+ match)
//! 3. **Levenshtein** - Fuzzy match with distance threshold
//!
//! This provides robust edit application that can handle various
//! edge cases in code modification.
//!
//! # Example
//!
//! ```
//! use pi::policy::edit_validation::{EditValidationChain, EditStrategy};
//!
//! let chain = EditValidationChain::new();
//! let result = chain.validate("old code", "new code", "file content with old code");
//!
//! if result.success {
//!     println!("Edit validated using {:?}", result.strategy_used);
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Result of an edit validation attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct EditResult {
    /// Whether the edit can be safely applied.
    pub success: bool,

    /// Strategy that was used for validation.
    pub strategy_used: EditStrategy,

    /// Confidence level (0.0 to 1.0).
    pub confidence: f32,

    /// Optional message explaining the result.
    pub message: Option<String>,

    /// Starting position of the match (if found).
    pub start_pos: Option<usize>,

    /// Ending position of the match (if found).
    pub end_pos: Option<usize>,
}

impl EditResult {
    /// Create a successful result.
    pub const fn success(strategy: EditStrategy, confidence: f32) -> Self {
        Self {
            success: true,
            strategy_used: strategy,
            confidence,
            message: None,
            start_pos: None,
            end_pos: None,
        }
    }

    /// Create a failed result.
    pub fn failure(strategy: EditStrategy, message: impl Into<String>) -> Self {
        Self {
            success: false,
            strategy_used: strategy,
            confidence: 0.0,
            message: Some(message.into()),
            start_pos: None,
            end_pos: None,
        }
    }

    /// Add position information.
    #[must_use]
    pub const fn with_position(mut self, start: usize, end: usize) -> Self {
        self.start_pos = Some(start);
        self.end_pos = Some(end);
        self
    }
}

/// Available edit validation strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditStrategy {
    /// Find by unique block markers (```, // EDIT:, etc.)
    BlockAnchor,

    /// Find by surrounding context using similarity matching.
    ContextAware,

    /// Fuzzy match using Levenshtein distance.
    Levenshtein,
}

impl std::fmt::Display for EditStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlockAnchor => write!(f, "block_anchor"),
            Self::ContextAware => write!(f, "context_aware"),
            Self::Levenshtein => write!(f, "levenshtein"),
        }
    }
}

/// Trait for edit validation strategies.
pub trait EditValidator: Send + Sync {
    /// Try to validate an edit, returning the result.
    ///
    /// # Arguments
    ///
    /// * `old` - The text to find and replace
    /// * `new` - The replacement text
    /// * `content` - The file content to search in
    ///
    /// # Returns
    ///
    /// `Some(EditResult)` if this strategy can handle the edit, `None` otherwise.
    fn validate(&self, old: &str, new: &str, content: &str) -> Option<EditResult>;

    /// Get the strategy type.
    fn strategy(&self) -> EditStrategy;
}

/// Chain of edit validators (tried in order).
///
/// This implements the OpenCode approach of trying multiple strategies
/// with fallback when one fails.
pub struct EditValidationChain {
    validators: Vec<Box<dyn EditValidator>>,
}

impl Default for EditValidationChain {
    fn default() -> Self {
        Self::new()
    }
}

impl EditValidationChain {
    /// Create a new chain with default validators.
    pub fn new() -> Self {
        Self {
            validators: vec![
                Box::new(BlockAnchorValidator),
                Box::new(ContextAwareValidator::new(0.5)),
                Box::new(LevenshteinValidator::new(0.3)),
            ],
        }
    }

    /// Create a chain with custom validators.
    pub fn with_validators(validators: Vec<Box<dyn EditValidator>>) -> Self {
        Self { validators }
    }

    /// Add a validator to the chain.
    pub fn add_validator<V: EditValidator + 'static>(&mut self, validator: V) {
        self.validators.push(Box::new(validator));
    }

    /// Try to validate an edit using all strategies in order.
    ///
    /// Returns the first successful result, or a failure if none succeed.
    pub fn validate(&self, old: &str, new: &str, content: &str) -> EditResult {
        // First, check for exact match (fast path)
        if let Some(pos) = content.find(old) {
            return EditResult::success(EditStrategy::BlockAnchor, 1.0)
                .with_position(pos, pos + old.len());
        }

        // Try each validator in order
        for validator in &self.validators {
            if let Some(result) = validator.validate(old, new, content) {
                if result.success {
                    return result;
                }
            }
        }

        // All strategies failed
        EditResult::failure(
            EditStrategy::Levenshtein,
            "No strategy could validate this edit - text not found in content",
        )
    }

    /// Get the best match position for an edit.
    pub fn find_best_match(&self, old: &str, content: &str) -> Option<(usize, usize, f32)> {
        let result = self.validate(old, "", content);
        if result.success {
            result
                .start_pos
                .zip(result.end_pos)
                .map(|(s, e)| (s, e, result.confidence))
        } else {
            None
        }
    }
}

// ============================================================================
// Block Anchor Validator
// ============================================================================

/// Block anchor validator - finds by unique markers.
///
/// This looks for unique markers like:
/// - ```block_id
/// - // EDIT: marker
/// - <!-- EDIT: marker -->
pub struct BlockAnchorValidator;

impl EditValidator for BlockAnchorValidator {
    fn validate(&self, old: &str, _new: &str, content: &str) -> Option<EditResult> {
        // Look for block anchors in the old text
        let anchor_patterns = ["```", "// EDIT:", "<!-- EDIT:", "# EDIT:", "/* EDIT:"];

        for pattern in &anchor_patterns {
            if old.contains(pattern) {
                // Try to find the entire block
                if let Some(pos) = content.find(old) {
                    return Some(
                        EditResult::success(EditStrategy::BlockAnchor, 1.0)
                            .with_position(pos, pos + old.len()),
                    );
                }

                // Try to find by the anchor pattern alone
                if let Some(start) = content.find(pattern) {
                    // Find the end of the block
                    let rest = &content[start..];
                    if let Some(end_offset) = rest.find(old.lines().last().unwrap_or("")) {
                        let end = start + end_offset + old.lines().last().unwrap_or("").len();
                        return Some(
                            EditResult::success(EditStrategy::BlockAnchor, 0.8)
                                .with_position(start, end),
                        );
                    }
                }
            }
        }

        None
    }

    fn strategy(&self) -> EditStrategy {
        EditStrategy::BlockAnchor
    }
}

// ============================================================================
// Context Aware Validator
// ============================================================================

/// Context-aware validator - matches by surrounding context.
///
/// Uses Jaccard similarity on word sets to find the best matching region.
pub struct ContextAwareValidator {
    /// Minimum similarity threshold (0.0 to 1.0).
    pub threshold: f32,
}

impl ContextAwareValidator {
    /// Create a new validator with the given threshold.
    pub const fn new(threshold: f32) -> Self {
        Self { threshold }
    }
}

impl EditValidator for ContextAwareValidator {
    fn validate(&self, old: &str, _new: &str, content: &str) -> Option<EditResult> {
        let similarity = calculate_jaccard_similarity(old, content);

        if similarity >= self.threshold {
            // Try to find the actual position
            if let Some(pos) = find_approximate_match(old, content, self.threshold) {
                return Some(
                    EditResult::success(EditStrategy::ContextAware, similarity)
                        .with_position(pos, pos + old.len()),
                );
            }

            // Return success without position
            return Some(EditResult::success(EditStrategy::ContextAware, similarity));
        }

        None
    }

    fn strategy(&self) -> EditStrategy {
        EditStrategy::ContextAware
    }
}

// ============================================================================
// Levenshtein Validator
// ============================================================================

/// Levenshtein-based fuzzy validator.
///
/// Uses Levenshtein distance to find approximate matches.
pub struct LevenshteinValidator {
    /// Maximum normalized distance threshold (0.0 to 1.0).
    pub max_distance: f32,
}

impl LevenshteinValidator {
    /// Create a new validator with the given max distance.
    pub const fn new(max_distance: f32) -> Self {
        Self { max_distance }
    }
}

impl EditValidator for LevenshteinValidator {
    fn validate(&self, old: &str, _new: &str, content: &str) -> Option<EditResult> {
        // For efficiency, only use Levenshtein on smaller texts
        if old.len() > 1000 || content.len() > 100_000 {
            return None;
        }

        // Try to find the best fuzzy match
        let best = find_best_fuzzy_match(old, content);

        if let Some((pos, score)) = best {
            if score >= (1.0 - self.max_distance) {
                return Some(
                    EditResult::success(EditStrategy::Levenshtein, score)
                        .with_position(pos, pos + old.len()),
                );
            }
        }

        None
    }

    fn strategy(&self) -> EditStrategy {
        EditStrategy::Levenshtein
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Calculate Jaccard similarity between two strings (on word sets).
fn calculate_jaccard_similarity(a: &str, b: &str) -> f32 {
    let words_a: HashSet<&str> = a.split_whitespace().collect();
    let words_b: HashSet<&str> = b.split_whitespace().collect();

    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

/// Find an approximate match position using sliding window.
fn find_approximate_match(needle: &str, haystack: &str, threshold: f32) -> Option<usize> {
    let needle_words: Vec<&str> = needle.split_whitespace().collect();
    let needle_set: HashSet<&str> = needle_words.iter().copied().collect();

    if needle_words.is_empty() {
        return None;
    }

    let haystack_words: Vec<&str> = haystack.split_whitespace().collect();
    let window_size = needle_words.len();

    // Slide a window through the haystack
    for i in 0..=haystack_words.len().saturating_sub(window_size) {
        let window: HashSet<&str> = haystack_words[i..i + window_size].iter().copied().collect();
        let similarity = calculate_jaccard_similarity_for_sets(&needle_set, &window);

        if similarity >= threshold {
            // Find the byte position
            let word_start = haystack_words[..i]
                .iter()
                .map(|w| w.len() + 1)
                .sum::<usize>();
            return Some(word_start);
        }
    }

    None
}

/// Calculate Jaccard similarity for pre-computed word sets.
fn calculate_jaccard_similarity_for_sets(a: &HashSet<&str>, b: &HashSet<&str>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }

    let intersection = a.intersection(b).count();
    let union = a.union(b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

/// Calculate Levenshtein distance between two strings.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();

    let a_len = a_chars.len();
    let b_len = b_chars.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut matrix = vec![vec![0; b_len + 1]; a_len + 1];

    for i in 0..=a_len {
        matrix[i][0] = i;
    }

    for j in 0..=b_len {
        matrix[0][j] = j;
    }

    for (i, a_char) in a_chars.iter().enumerate() {
        for (j, b_char) in b_chars.iter().enumerate() {
            let cost = usize::from(a_char != b_char);
            matrix[i + 1][j + 1] = (matrix[i][j + 1] + 1)
                .min(matrix[i + 1][j] + 1)
                .min(matrix[i][j] + cost);
        }
    }

    matrix[a_len][b_len]
}

/// Find the best fuzzy match position using Levenshtein distance.
fn find_best_fuzzy_match(needle: &str, haystack: &str) -> Option<(usize, f32)> {
    let needle_len = needle.len();
    let haystack_len = haystack.len();

    if needle_len == 0 || haystack_len == 0 || needle_len > haystack_len {
        return None;
    }

    let mut best_pos = 0;
    let mut best_score = 0.0_f32;

    // Slide through haystack with some overlap for efficiency
    let step = (needle_len / 4).max(1);

    for pos in (0..haystack_len.saturating_sub(needle_len)).step_by(step) {
        let end = (pos + needle_len).min(haystack_len);
        let candidate = &haystack[pos..end];

        let distance = levenshtein_distance(needle, candidate);
        let max_len = needle_len.max(candidate.len()) as f32;
        let normalized = 1.0 - (distance as f32 / max_len);

        if normalized > best_score {
            best_score = normalized;
            best_pos = pos;
        }
    }

    if best_score > 0.0 {
        Some((best_pos, best_score))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        let chain = EditValidationChain::new();
        let content = "Hello world, this is a test";
        let result = chain.validate("Hello world", "Hi there", content);

        assert!(result.success);
        assert!((result.confidence - 1.0).abs() < f32::EPSILON);
        assert_eq!(result.start_pos, Some(0));
        assert_eq!(result.end_pos, Some(11));
    }

    #[test]
    fn test_block_anchor_match() {
        let validator = BlockAnchorValidator;
        let old = "```rust\nfn main() {}\n```";
        let content = "Some text\n```rust\nfn main() {}\n```\nMore text";

        let result = validator.validate(old, "new", content);
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.success);
    }

    #[test]
    fn test_context_aware_match() {
        let validator = ContextAwareValidator::new(0.3);
        let old = "function hello world test";
        let content = "some code function hello world test more code";

        let result = validator.validate(old, "new", content);
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.success);
        assert!(result.confidence >= 0.3);
    }

    #[test]
    fn test_levenshtein_match() {
        let validator = LevenshteinValidator::new(0.3);
        let old = "hello world";
        let content = "hello worl"; // Missing 'd'

        let result = validator.validate(old, "new", content);
        // Should not match because content is shorter
        assert!(result.is_none() || !result.unwrap().success);
    }

    #[test]
    fn test_jaccard_similarity() {
        let sim = calculate_jaccard_similarity("hello world", "hello world");
        assert!((sim - 1.0).abs() < f32::EPSILON);

        let sim = calculate_jaccard_similarity("hello world", "hello there");
        assert!(sim > 0.3 && sim < 0.7);

        let sim = calculate_jaccard_similarity("abc", "xyz");
        assert!((sim - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("hello", ""), 5);
        assert_eq!(levenshtein_distance("", "hello"), 5);
        assert_eq!(levenshtein_distance("hello", "hello"), 0);
        assert_eq!(levenshtein_distance("hello", "helo"), 1);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn test_chain_fallback() {
        let chain = EditValidationChain::new();

        // Text that doesn't exist exactly
        let content = "The quick brown fox jumps";
        let result = chain.validate("fast brown cat", "slow white dog", content);

        // Should try strategies and potentially fail
        // (depending on similarity threshold)
        assert!(!result.success || result.confidence < 1.0);
    }

    #[test]
    fn test_edit_result_builder() {
        let result = EditResult::success(EditStrategy::BlockAnchor, 0.9).with_position(10, 20);

        assert!(result.success);
        assert_eq!(result.strategy_used, EditStrategy::BlockAnchor);
        assert!((result.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(result.start_pos, Some(10));
        assert_eq!(result.end_pos, Some(20));
    }

    #[test]
    fn test_edit_strategy_display() {
        assert_eq!(EditStrategy::BlockAnchor.to_string(), "block_anchor");
        assert_eq!(EditStrategy::ContextAware.to_string(), "context_aware");
        assert_eq!(EditStrategy::Levenshtein.to_string(), "levenshtein");
    }

    #[test]
    fn test_edit_strategy_serde() {
        let strategy = EditStrategy::ContextAware;
        let json = serde_json::to_string(&strategy).unwrap();
        assert_eq!(json, "\"context_aware\"");
        let parsed: EditStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, strategy);
    }
}
