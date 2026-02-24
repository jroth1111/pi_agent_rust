//! Model-specific context thresholds (Cline approach).
//!
//! Different models have different context windows and require different
//! thresholds to prevent degradation. This module implements the Cline
//! approach of model-specific buffer sizes:
//!
//! - 64k context → 37k threshold (~58%)
//! - 128k context → 98k threshold (~77%)
//! - 200k context → 160k threshold (~80%)
//! - 1M+ context → 80% of window
//!
//! # Example
//!
//! ```
//! use pi::context::threshold::{get_context_threshold, get_context_window};
//!
//! // For a model with 200k context window
//! let threshold = get_context_threshold_for_window(200_000);
//! assert_eq!(threshold, 160_000);
//!
//! // For a model with 64k context window
//! let threshold = get_context_threshold_for_window(64_000);
//! assert_eq!(threshold, 37_000);
//! ```

use crate::provider::Model;

/// Context window sizes for known model patterns.
///
/// Used for fallback when `Model.context_window` is not set.
pub const CONTEXT_WINDOW_PATTERNS: &[(&str, usize)] = &[
    // DeepSeek models - 64k context
    ("deepseek", 64_000),
    // GPT-4 models - 128k context
    ("gpt-4", 128_000),
    ("gpt-4o", 128_000),
    ("gpt-4-turbo", 128_000),
    // Claude 3 models - 200k context
    ("claude-3", 200_000),
    ("claude-sonnet", 200_000),
    ("claude-opus", 200_000),
    ("claude-haiku", 200_000),
    // Gemini models - 1M+ context
    ("gemini-1.5", 1_048_576),
    ("gemini-2.0", 1_048_576),
    ("gemini-2.5", 1_048_576),
    // Llama models
    ("llama-3", 128_000),
    // Mistral models
    ("mistral-large", 128_000),
    ("codestral", 128_000),
];

/// Get context threshold for a model (Cline approach: model-specific buffers).
///
/// This returns the maximum tokens that should be used before triggering
/// compaction, based on the model's context window size.
///
/// # Thresholds by Window Size
///
/// | Window Size | Threshold | Percentage |
/// |-------------|-----------|------------|
/// | 64k         | 37k       | ~58%       |
/// | 128k        | 98k       | ~77%       |
/// | 200k        | 160k      | ~80%       |
/// | 1M+         | 80%       | 80%        |
///
/// # Example
///
/// ```
/// use pi::context::threshold::get_context_threshold_for_window;
///
/// let threshold = get_context_threshold_for_window(200_000);
/// assert_eq!(threshold, 160_000);
/// ```
pub fn get_context_threshold_for_window(context_window: usize) -> usize {
    match context_window {
        // DeepSeek-style 64k context
        64_000 => 37_000,
        // Standard 128k context (GPT-4, Claude-3-Sonnet, etc.)
        128_000 => 98_000,
        // Extended 200k context (Claude-3-Opus, Claude-4)
        200_000 => 160_000,
        // 1M+ context (Gemini) - use 80%
        n if n >= 1_000_000 => (n as f32 * 0.8) as usize,
        // Default: 75% of window
        n => (n as f32 * 0.75) as usize,
    }
}

/// Get context threshold for a Model instance.
///
/// Uses the model's `context_window` field if set, otherwise falls back
/// to pattern matching on the model ID.
///
/// # Example
///
/// ```ignore
/// use pi::context::threshold::get_context_threshold;
///
/// let model = get_model_from_somewhere();
/// let threshold = get_context_threshold(&model);
/// println!("Compaction threshold: {} tokens", threshold);
/// ```
pub fn get_context_threshold(model: &Model) -> usize {
    let window = get_context_window(model);
    get_context_threshold_for_window(window)
}

/// Get context window size for a model.
///
/// Checks `model.context_window` first, then falls back to pattern matching
/// on the model ID.
///
/// # Example
///
/// ```
/// use pi::context::threshold::get_context_window_for_id;
///
/// let window = get_context_window_for_id("claude-sonnet-4-20250514");
/// assert_eq!(window, 200_000);
///
/// let window = get_context_window_for_id("gpt-4o");
/// assert_eq!(window, 128_000);
/// ```
pub fn get_context_window(model: &Model) -> usize {
    // Use model's context_window if set and reasonable
    if model.context_window > 0 {
        return model.context_window as usize;
    }

    // Fall back to pattern matching
    get_context_window_for_id(&model.id)
}

/// Get context window size for a model ID string.
///
/// Uses pattern matching to determine the context window based on
/// the model ID.
pub fn get_context_window_for_id(model_id: &str) -> usize {
    let id_lower = model_id.to_lowercase();

    for (pattern, window) in CONTEXT_WINDOW_PATTERNS {
        if id_lower.contains(pattern) {
            return *window;
        }
    }

    // Default to 128k
    128_000
}

/// Calculate the safe buffer size for a model.
///
/// This is the number of tokens that should be kept free for response
/// generation and overhead.
///
/// # Example
///
/// ```
/// use pi::context::threshold::get_safe_buffer;
///
/// let buffer = get_safe_buffer(200_000);
/// assert_eq!(buffer, 40_000); // 20% of 200k
/// ```
pub fn get_safe_buffer(context_window: usize) -> usize {
    context_window.saturating_sub(get_context_threshold_for_window(context_window))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_threshold_for_64k_window() {
        let threshold = get_context_threshold_for_window(64_000);
        assert_eq!(threshold, 37_000);
    }

    #[test]
    fn test_threshold_for_128k_window() {
        let threshold = get_context_threshold_for_window(128_000);
        assert_eq!(threshold, 98_000);
    }

    #[test]
    fn test_threshold_for_200k_window() {
        let threshold = get_context_threshold_for_window(200_000);
        assert_eq!(threshold, 160_000);
    }

    #[test]
    fn test_threshold_for_1m_window() {
        let threshold = get_context_threshold_for_window(1_048_576);
        assert_eq!(threshold, (1_048_576_f32 * 0.8) as usize);
    }

    #[test]
    fn test_threshold_for_unknown_window() {
        // Unknown windows get 75%
        let threshold = get_context_threshold_for_window(100_000);
        assert_eq!(threshold, 75_000);
    }

    #[test]
    fn test_context_window_for_id_claude() {
        assert_eq!(
            get_context_window_for_id("claude-sonnet-4-20250514"),
            200_000
        );
        assert_eq!(get_context_window_for_id("claude-opus-4-5"), 200_000);
        assert_eq!(get_context_window_for_id("claude-3-5-sonnet"), 200_000);
    }

    #[test]
    fn test_context_window_for_id_gpt4() {
        assert_eq!(get_context_window_for_id("gpt-4o"), 128_000);
        assert_eq!(get_context_window_for_id("gpt-4-turbo"), 128_000);
    }

    #[test]
    fn test_context_window_for_id_deepseek() {
        assert_eq!(get_context_window_for_id("deepseek-coder"), 64_000);
        assert_eq!(get_context_window_for_id("deepseek-chat"), 64_000);
    }

    #[test]
    fn test_context_window_for_id_gemini() {
        assert_eq!(get_context_window_for_id("gemini-2.5-pro"), 1_048_576);
        assert_eq!(get_context_window_for_id("gemini-1.5-flash"), 1_048_576);
    }

    #[test]
    fn test_context_window_for_id_unknown() {
        assert_eq!(get_context_window_for_id("unknown-model"), 128_000);
    }

    #[test]
    fn test_safe_buffer() {
        // 200k window: 200k - 160k = 40k buffer
        assert_eq!(get_safe_buffer(200_000), 40_000);

        // 128k window: 128k - 98k = 30k buffer
        assert_eq!(get_safe_buffer(128_000), 30_000);

        // 64k window: 64k - 37k = 27k buffer
        assert_eq!(get_safe_buffer(64_000), 27_000);
    }

    #[test]
    fn test_threshold_percentages() {
        // Verify the percentages match the Cline approach

        // 64k: 37k/64k = ~58%
        let ratio_64k = get_context_threshold_for_window(64_000) as f32 / 64_000_f32;
        assert!((ratio_64k - 0.578).abs() < 0.01);

        // 128k: 98k/128k = ~77%
        let ratio_128k = get_context_threshold_for_window(128_000) as f32 / 128_000_f32;
        assert!((ratio_128k - 0.766).abs() < 0.01);

        // 200k: 160k/200k = 80%
        let ratio_200k = get_context_threshold_for_window(200_000) as f32 / 200_000_f32;
        assert!((ratio_200k - 0.8).abs() < 0.01);

        // 1M: 80%
        let ratio_1m = get_context_threshold_for_window(1_000_000) as f32 / 1_000_000_f32;
        assert!((ratio_1m - 0.8).abs() < 0.01);
    }
}
