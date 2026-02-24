//! Entropy-based rotation triggers.
//!
//! This module implements Shannon entropy calculation for OAuth token usage
//! patterns. Lower entropy indicates more predictable patterns (e.g., consistent
//! failures), which may trigger rotation.

use crate::auth::OAuthErrorClass;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// A single usage sample for entropy calculation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSample {
    /// Timestamp of the sample (Unix milliseconds).
    pub timestamp_ms: i64,

    /// Whether the request succeeded.
    pub success: bool,

    /// Request latency in milliseconds.
    pub latency_ms: u64,

    /// Error classification if failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<OAuthErrorClass>,
}

impl UsageSample {
    /// Create a new usage sample.
    pub const fn new(timestamp_ms: i64, success: bool, latency_ms: u64) -> Self {
        Self {
            timestamp_ms,
            success,
            latency_ms,
            error_class: None,
        }
    }

    /// Create a sample for a successful request.
    pub const fn success(timestamp_ms: i64, latency_ms: u64) -> Self {
        Self::new(timestamp_ms, true, latency_ms)
    }

    /// Create a sample for a failed request.
    pub const fn failure(timestamp_ms: i64, latency_ms: u64, error_class: OAuthErrorClass) -> Self {
        Self {
            timestamp_ms,
            success: false,
            latency_ms,
            error_class: Some(error_class),
        }
    }
}

/// Calculator for usage pattern entropy.
///
/// Uses Shannon entropy to measure the unpredictability of success/failure
/// patterns. Lower entropy indicates more predictable patterns (e.g., all failures),
/// which may warrant rotation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntropyCalculator {
    /// Circular buffer of usage samples.
    samples: VecDeque<UsageSample>,

    /// Maximum number of samples to retain.
    max_samples: usize,

    /// Running count of successes (for optimization).
    #[serde(default)]
    success_count: u64,

    /// Running count of failures (for optimization).
    #[serde(default)]
    failure_count: u64,
}

impl EntropyCalculator {
    /// Create a new entropy calculator with the given maximum sample count.
    pub fn new(max_samples: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(max_samples),
            max_samples,
            success_count: 0,
            failure_count: 0,
        }
    }

    /// Get the maximum number of samples.
    pub const fn max_samples(&self) -> usize {
        self.max_samples
    }

    /// Get the current number of samples.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Get the success count.
    pub const fn success_count(&self) -> u64 {
        self.success_count
    }

    /// Get the failure count.
    pub const fn failure_count(&self) -> u64 {
        self.failure_count
    }

    /// Add a new sample to the calculator.
    ///
    /// If the buffer is full, the oldest sample is removed.
    pub fn add_sample(&mut self, sample: UsageSample) {
        // If at capacity, remove the oldest sample
        if self.samples.len() >= self.max_samples {
            if let Some(oldest) = self.samples.pop_front() {
                if oldest.success {
                    self.success_count = self.success_count.saturating_sub(1);
                } else {
                    self.failure_count = self.failure_count.saturating_sub(1);
                }
            }
        }

        // Update counts
        if sample.success {
            self.success_count += 1;
        } else {
            self.failure_count += 1;
        }

        self.samples.push_back(sample);
    }

    /// Record a successful request.
    pub fn record_success(&mut self, timestamp_ms: i64, latency_ms: u64) {
        self.add_sample(UsageSample::success(timestamp_ms, latency_ms));
    }

    /// Record a failed request.
    pub fn record_failure(
        &mut self,
        timestamp_ms: i64,
        latency_ms: u64,
        error_class: OAuthErrorClass,
    ) {
        self.add_sample(UsageSample::failure(timestamp_ms, latency_ms, error_class));
    }

    /// Calculate Shannon entropy of the success/failure distribution.
    ///
    /// Returns a value between 0.0 (completely predictable) and 1.0 (maximum
    /// unpredictability). For binary outcomes (success/failure), maximum entropy
    /// is achieved when both are equally likely.
    ///
    /// # Formula
    /// H = -p_success * log2(p_success) - p_failure * log2(p_failure)
    pub fn calculate(&self) -> f64 {
        let total = self.success_count + self.failure_count;
        if total == 0 {
            // No samples = maximum uncertainty
            return 1.0;
        }

        let p_success = self.success_count as f64 / total as f64;
        let p_failure = self.failure_count as f64 / total as f64;

        // Shannon entropy for binary distribution
        let h_success = if p_success > 0.0 {
            -p_success * p_success.log2()
        } else {
            0.0
        };

        let h_failure = if p_failure > 0.0 {
            -p_failure * p_failure.log2()
        } else {
            0.0
        };

        // Normalize by log2(2) = 1, so max is 1.0
        h_success + h_failure
    }

    /// Check if rotation should be triggered based on entropy threshold.
    ///
    /// Returns true when entropy drops below the threshold, indicating
    /// predictable (potentially problematic) patterns.
    pub fn should_rotate(&self, threshold: f64) -> bool {
        let entropy = self.calculate();
        entropy < threshold && self.sample_count() >= 10
    }

    /// Clear all samples and reset counters.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.success_count = 0;
        self.failure_count = 0;
    }

    /// Get the samples as a slice.
    pub const fn samples(&self) -> &VecDeque<UsageSample> {
        &self.samples
    }

    /// Convert samples to a vector (for serialization).
    pub fn samples_vec(&self) -> Vec<UsageSample> {
        self.samples.iter().cloned().collect()
    }

    /// Load samples from a vector.
    pub fn load_samples(&mut self, samples: Vec<UsageSample>) {
        self.clear();
        for sample in samples {
            self.add_sample(sample);
        }
    }

    /// Get the success rate as a percentage (0.0-100.0).
    pub fn success_rate(&self) -> f64 {
        let total = self.success_count + self.failure_count;
        if total == 0 {
            return 100.0; // Assume success when no data
        }
        (self.success_count as f64 / total as f64) * 100.0
    }

    /// Get statistics about the samples.
    pub fn stats(&self) -> EntropyStats {
        let total = self.success_count + self.failure_count;
        let avg_latency = if self.samples.is_empty() {
            0.0
        } else {
            self.samples
                .iter()
                .map(|s| s.latency_ms as f64)
                .sum::<f64>()
                / self.samples.len() as f64
        };

        EntropyStats {
            sample_count: total,
            success_count: self.success_count,
            failure_count: self.failure_count,
            entropy: self.calculate(),
            success_rate: self.success_rate(),
            average_latency_ms: avg_latency,
        }
    }
}

/// Statistics about entropy calculation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EntropyStats {
    /// Total number of samples.
    pub sample_count: u64,

    /// Number of successful samples.
    pub success_count: u64,

    /// Number of failed samples.
    pub failure_count: u64,

    /// Current entropy value (0.0-1.0).
    pub entropy: f64,

    /// Success rate as percentage (0.0-100.0).
    pub success_rate: f64,

    /// Average request latency in milliseconds.
    pub average_latency_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entropy_calculator_new() {
        let calc = EntropyCalculator::new(100);
        assert_eq!(calc.max_samples(), 100);
        assert_eq!(calc.sample_count(), 0);
        assert_eq!(calc.success_count(), 0);
        assert_eq!(calc.failure_count(), 0);
    }

    #[test]
    fn test_add_sample() {
        let mut calc = EntropyCalculator::new(10);
        calc.add_sample(UsageSample::success(1000, 100));

        assert_eq!(calc.sample_count(), 1);
        assert_eq!(calc.success_count(), 1);
        assert_eq!(calc.failure_count(), 0);
    }

    #[test]
    fn test_max_samples_limit() {
        let mut calc = EntropyCalculator::new(3);

        calc.add_sample(UsageSample::success(1000, 100));
        calc.add_sample(UsageSample::success(2000, 100));
        calc.add_sample(UsageSample::success(3000, 100));
        calc.add_sample(UsageSample::failure(4000, 100, OAuthErrorClass::Auth));

        assert_eq!(calc.sample_count(), 3);
        assert_eq!(calc.success_count(), 2);
        assert_eq!(calc.failure_count(), 1);
    }

    #[test]
    fn test_entropy_no_samples() {
        let calc = EntropyCalculator::new(100);
        // No samples = maximum uncertainty
        assert_eq!(calc.calculate(), 1.0);
    }

    #[test]
    fn test_entropy_all_successes() {
        let mut calc = EntropyCalculator::new(100);
        for i in 0..10 {
            calc.add_sample(UsageSample::success(i * 1000, 100));
        }

        // All successes = 0 entropy (completely predictable)
        assert_eq!(calc.calculate(), 0.0);
        assert_eq!(calc.success_rate(), 100.0);
    }

    #[test]
    fn test_entropy_all_failures() {
        let mut calc = EntropyCalculator::new(100);
        for i in 0..10 {
            calc.add_sample(UsageSample::failure(i * 1000, 100, OAuthErrorClass::Auth));
        }

        // All failures = 0 entropy (completely predictable)
        assert_eq!(calc.calculate(), 0.0);
        assert_eq!(calc.success_rate(), 0.0);
    }

    #[test]
    fn test_entropy_half_half() {
        let mut calc = EntropyCalculator::new(100);
        for i in 0..10 {
            if i % 2 == 0 {
                calc.add_sample(UsageSample::success(i * 1000, 100));
            } else {
                calc.add_sample(UsageSample::failure(i * 1000, 100, OAuthErrorClass::Auth));
            }
        }

        // 50/50 = maximum entropy for binary distribution
        // Entropy should be exactly 1.0 for binary
        let entropy = calc.calculate();
        assert!((entropy - 1.0).abs() < 0.0001);
        assert_eq!(calc.success_rate(), 50.0);
    }

    #[test]
    fn test_entropy_75_25() {
        let mut calc = EntropyCalculator::new(100);
        // 75% success, 25% failure
        for i in 0..100 {
            if i < 75 {
                calc.add_sample(UsageSample::success(i * 1000, 100));
            } else {
                calc.add_sample(UsageSample::failure(
                    i * 1000,
                    100,
                    OAuthErrorClass::RateLimit,
                ));
            }
        }

        let entropy = calc.calculate();
        // H = -0.75 * log2(0.75) - 0.25 * log2(0.25)
        // H ≈ 0.811
        assert!((entropy - 0.811).abs() < 0.01);
    }

    #[test]
    fn test_should_rotate() {
        let mut calc = EntropyCalculator::new(100);

        // Not enough samples
        for i in 0..5 {
            calc.add_sample(UsageSample::success(i * 1000, 100));
        }
        assert!(!calc.should_rotate(0.5)); // Need at least 10 samples

        // Add more successes - entropy is 0 but not enough samples initially
        for i in 5..15 {
            calc.add_sample(UsageSample::success(i * 1000, 100));
        }

        // Now we have 15 samples, all successes, entropy = 0
        assert!(calc.should_rotate(0.5)); // 0 < 0.5
        assert!(!calc.should_rotate(0.0)); // 0 is not < 0
    }

    #[test]
    fn test_clear() {
        let mut calc = EntropyCalculator::new(100);
        calc.add_sample(UsageSample::success(1000, 100));
        calc.add_sample(UsageSample::failure(2000, 100, OAuthErrorClass::Auth));

        calc.clear();

        assert_eq!(calc.sample_count(), 0);
        assert_eq!(calc.success_count(), 0);
        assert_eq!(calc.failure_count(), 0);
    }

    #[test]
    fn test_samples_roundtrip() {
        let mut calc = EntropyCalculator::new(100);
        calc.add_sample(UsageSample::success(1000, 100));
        calc.add_sample(UsageSample::failure(2000, 200, OAuthErrorClass::Auth));
        calc.add_sample(UsageSample::success(3000, 150));

        let samples = calc.samples_vec();

        let mut calc2 = EntropyCalculator::new(100);
        calc2.load_samples(samples);

        assert_eq!(calc.sample_count(), calc2.sample_count());
        assert_eq!(calc.success_count(), calc2.success_count());
        assert_eq!(calc.failure_count(), calc2.failure_count());
        assert!((calc.calculate() - calc2.calculate()).abs() < 0.0001);
    }

    #[test]
    fn test_stats() {
        let mut calc = EntropyCalculator::new(100);
        calc.add_sample(UsageSample::success(1000, 100));
        calc.add_sample(UsageSample::success(2000, 200));
        calc.add_sample(UsageSample::failure(3000, 300, OAuthErrorClass::Auth));

        let stats = calc.stats();
        assert_eq!(stats.sample_count, 3);
        assert_eq!(stats.success_count, 2);
        assert_eq!(stats.failure_count, 1);
        assert!((stats.success_rate - 66.666).abs() < 0.1);
        assert!((stats.average_latency_ms - 200.0).abs() < 0.1);
    }

    #[test]
    fn test_record_methods() {
        let mut calc = EntropyCalculator::new(100);

        calc.record_success(1000, 50);
        assert_eq!(calc.success_count(), 1);

        calc.record_failure(2000, 100, OAuthErrorClass::RateLimit);
        assert_eq!(calc.failure_count(), 1);

        let sample = calc.samples().front().unwrap();
        assert!(sample.success);
        assert_eq!(sample.latency_ms, 50);
    }
}
