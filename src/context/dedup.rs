//! Cline-style read deduplication for context management.
//!
//! This module implements file read deduplication to prevent the same file
//! content from appearing multiple times in the context window. When a file
//! is read multiple times, a small notice is returned instead of the full
//! content, referencing the original read.
//!
//! # Example
//!
//! ```
//! use pi::context::dedup::DuplicateReadDetector;
//! use std::path::Path;
//!
//! let mut detector = DuplicateReadDetector::new();
//!
//! // First read returns None (not a duplicate)
//! let path = Path::new("/src/main.rs");
//! let result = detector.detect_duplicate_read(path);
//! assert!(result.is_none());
//!
//! // Second read returns a notice
//! let result = detector.detect_duplicate_read(path);
//! assert!(result.is_some());
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique identifier for a read operation.
pub type ReadId = u64;

/// Context tier for prioritization of file reads.
///
/// Files in hotter tiers are more likely to be kept in context
/// during compaction operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ContextTier {
    /// Hot tier: actively edited or critical files (highest priority).
    /// These files should never be deduplicated or compacted.
    Hot,
    /// Warm tier: recently read files that may be referenced again.
    /// Standard deduplication applies with a grace period.
    #[default]
    Warm,
    /// Cold tier: old reads that can be safely deduplicated.
    /// Aggressive deduplication applies.
    Cold,
}

impl ContextTier {
    /// Returns the priority value for this tier (higher = more important).
    pub const fn priority(&self) -> u8 {
        match self {
            Self::Hot => 100,
            Self::Warm => 50,
            Self::Cold => 10,
        }
    }
}

/// Notice returned when a duplicate file read is detected.
///
/// Contains metadata about the original read operation, allowing
/// the agent to reference previous context if needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateNotice {
    /// Unique identifier of the original read operation.
    pub original_read_id: ReadId,
    /// Path of the file that was read.
    pub path: PathBuf,
    /// Timestamp of the original read (Unix epoch milliseconds).
    pub timestamp: u64,
    /// Number of times this file has been read (including current).
    pub read_count: u32,
    /// Current tier of this file in the context.
    pub tier: ContextTier,
}

impl DuplicateNotice {
    /// Format the notice as a human-readable string for context inclusion.
    pub fn to_context_string(&self) -> String {
        format!(
            "[File already read: {} (read {} time{}, original read #{})]",
            self.path.display(),
            self.read_count,
            if self.read_count > 1 { "s" } else { "" },
            self.original_read_id
        )
    }
}

/// Metadata for a tracked file read.
#[derive(Debug, Clone)]
struct ReadMetadata {
    /// Unique identifier for this read.
    id: ReadId,
    /// Tier of this file in context.
    tier: ContextTier,
    /// Timestamp when the file was first read.
    first_read: u64,
    /// Number of times this file has been read.
    read_count: u32,
}

/// Detector for duplicate file reads (Cline approach).
///
/// Tracks file paths read during the current session and returns
/// a small notice instead of full content when the same file is
/// read multiple times.
///
/// # Thread Safety
///
/// This detector is not thread-safe. Use separate instances or wrap
/// in appropriate synchronization primitives for concurrent access.
#[derive(Debug, Clone)]
pub struct DuplicateReadDetector {
    /// Map of canonical file paths to their read metadata.
    reads: HashMap<PathBuf, ReadMetadata>,
    /// Counter for generating unique read IDs.
    next_id: ReadId,
    /// Whether to skip deduplication for hot-tier files.
    preserve_hot: bool,
}

impl Default for DuplicateReadDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl DuplicateReadDetector {
    /// Create a new duplicate read detector.
    #[must_use]
    pub fn new() -> Self {
        Self {
            reads: HashMap::new(),
            next_id: 1,
            preserve_hot: true,
        }
    }

    /// Create a detector with custom settings.
    ///
    /// # Arguments
    /// * `preserve_hot` - If true, hot-tier files are never deduplicated.
    #[must_use]
    pub fn with_settings(preserve_hot: bool) -> Self {
        Self {
            reads: HashMap::new(),
            next_id: 1,
            preserve_hot,
        }
    }

    /// Detect if a file read is a duplicate.
    ///
    /// Returns `None` if this is the first read of the file (content should
    /// be included in full). Returns `Some(DuplicateNotice)` if the file
    /// has been read before (only a notice should be included).
    ///
    /// # Arguments
    /// * `path` - The path being read
    ///
    /// # Returns
    /// - `None` for first reads
    /// - `Some(DuplicateNotice)` for subsequent reads
    pub fn detect_duplicate_read(&mut self, path: &Path) -> Option<DuplicateNotice> {
        let now = current_timestamp();

        // Try to canonicalize the path for consistent matching
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if let Some(metadata) = self.reads.get_mut(&canonical_path) {
            // File has been read before
            metadata.read_count += 1;

            // If preserve_hot is enabled and this is a hot file, don't deduplicate
            if self.preserve_hot && metadata.tier == ContextTier::Hot {
                return None;
            }

            Some(DuplicateNotice {
                original_read_id: metadata.id,
                path: canonical_path.clone(),
                timestamp: metadata.first_read,
                read_count: metadata.read_count,
                tier: metadata.tier,
            })
        } else {
            // First read of this file
            let id = self.next_id;
            self.next_id += 1;

            self.reads.insert(
                canonical_path.clone(),
                ReadMetadata {
                    id,
                    tier: ContextTier::default(),
                    first_read: now,
                    read_count: 1,
                },
            );

            None
        }
    }

    /// Register a file read with an explicit tier.
    ///
    /// Use this when you know the priority of a file (e.g., actively editing).
    ///
    /// # Arguments
    /// * `path` - The path being read
    /// * `tier` - The context tier for this file
    pub fn register_read_with_tier(&mut self, path: &Path, tier: ContextTier) {
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if let Some(metadata) = self.reads.get_mut(&canonical_path) {
            // Upgrade tier if the new tier is higher priority
            if tier.priority() > metadata.tier.priority() {
                metadata.tier = tier;
            }
            metadata.read_count += 1;
        } else {
            let id = self.next_id;
            self.next_id += 1;

            self.reads.insert(
                canonical_path,
                ReadMetadata {
                    id,
                    tier,
                    first_read: current_timestamp(),
                    read_count: 1,
                },
            );
        }
    }

    /// Update the tier for a tracked file.
    ///
    /// This can be used to promote a file to hot when actively editing,
    /// or demote to cold when no longer needed.
    pub fn update_tier(&mut self, path: &Path, tier: ContextTier) -> bool {
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if let Some(metadata) = self.reads.get_mut(&canonical_path) {
            metadata.tier = tier;
            true
        } else {
            false
        }
    }

    /// Get the current tier for a file, if tracked.
    pub fn get_tier(&self, path: &Path) -> Option<ContextTier> {
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.reads.get(&canonical_path).map(|m| m.tier)
    }

    /// Get the read count for a file, if tracked.
    pub fn get_read_count(&self, path: &Path) -> u32 {
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.reads.get(&canonical_path).map_or(0, |m| m.read_count)
    }

    /// Check if a file has been read before.
    pub fn has_been_read(&self, path: &Path) -> bool {
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.reads.contains_key(&canonical_path)
    }

    /// Get the total number of unique files tracked.
    pub fn unique_file_count(&self) -> usize {
        self.reads.len()
    }

    /// Get all tracked file paths.
    pub fn tracked_paths(&self) -> Vec<PathBuf> {
        self.reads.keys().cloned().collect()
    }

    /// Clear all tracked reads.
    ///
    /// Use this when starting a new session or task.
    pub fn clear(&mut self) {
        self.reads.clear();
        self.next_id = 1;
    }

    /// Remove cold-tier files to free up tracking memory.
    ///
    /// Returns the number of files removed.
    pub fn prune_cold(&mut self) -> usize {
        let initial_count = self.reads.len();
        self.reads.retain(|_, m| m.tier != ContextTier::Cold);
        initial_count - self.reads.len()
    }

    /// Get statistics about tracked files by tier.
    pub fn tier_stats(&self) -> TierStats {
        let mut stats = TierStats::default();
        for metadata in self.reads.values() {
            match metadata.tier {
                ContextTier::Hot => stats.hot_count += 1,
                ContextTier::Warm => stats.warm_count += 1,
                ContextTier::Cold => stats.cold_count += 1,
            }
            stats.total_reads += metadata.read_count;
        }
        stats
    }
}

/// Statistics about files in each tier.
#[derive(Debug, Clone, Copy, Default)]
pub struct TierStats {
    /// Number of hot-tier files.
    pub hot_count: usize,
    /// Number of warm-tier files.
    pub warm_count: usize,
    /// Number of cold-tier files.
    pub cold_count: usize,
    /// Total read operations across all files.
    pub total_reads: u32,
}

/// Get the current timestamp in milliseconds since Unix epoch.
fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn first_read_returns_none() {
        let mut detector = DuplicateReadDetector::new();
        let path = Path::new("/some/file.rs");

        let result = detector.detect_duplicate_read(path);
        assert!(result.is_none());
    }

    #[test]
    fn second_read_returns_notice() {
        let mut detector = DuplicateReadDetector::new();
        let path = Path::new("/some/file.rs");

        // First read
        detector.detect_duplicate_read(path);

        // Second read
        let result = detector.detect_duplicate_read(path);
        assert!(result.is_some());

        let notice = result.unwrap();
        assert_eq!(notice.read_count, 2);
        assert_eq!(notice.tier, ContextTier::Warm);
    }

    #[test]
    fn read_count_increments() {
        let mut detector = DuplicateReadDetector::new();
        let path = Path::new("/some/file.rs");

        detector.detect_duplicate_read(path);
        detector.detect_duplicate_read(path);
        detector.detect_duplicate_read(path);

        assert_eq!(detector.get_read_count(path), 3);
    }

    #[test]
    fn hot_tier_not_deduplicated_by_default() {
        let mut detector = DuplicateReadDetector::new();
        let path = Path::new("/hot/file.rs");

        // Register as hot
        detector.register_read_with_tier(path, ContextTier::Hot);

        // Even on second read, returns None because it's hot
        let result = detector.detect_duplicate_read(path);
        assert!(result.is_none());
    }

    #[test]
    fn hot_tier_deduplicated_when_preserve_hot_false() {
        let mut detector = DuplicateReadDetector::with_settings(false);
        let path = Path::new("/hot/file.rs");

        detector.register_read_with_tier(path, ContextTier::Hot);
        let result = detector.detect_duplicate_read(path);
        assert!(result.is_some());
    }

    #[test]
    fn tier_upgrade() {
        let mut detector = DuplicateReadDetector::new();
        let path = Path::new("/some/file.rs");

        // First read as warm (default)
        detector.detect_duplicate_read(path);
        assert_eq!(detector.get_tier(path), Some(ContextTier::Warm));

        // Upgrade to hot
        detector.register_read_with_tier(path, ContextTier::Hot);
        assert_eq!(detector.get_tier(path), Some(ContextTier::Hot));
    }

    #[test]
    fn tier_downgrade() {
        let mut detector = DuplicateReadDetector::new();
        let path = Path::new("/some/file.rs");

        detector.register_read_with_tier(path, ContextTier::Hot);
        assert_eq!(detector.get_tier(path), Some(ContextTier::Hot));

        // Downgrade to cold
        let updated = detector.update_tier(path, ContextTier::Cold);
        assert!(updated);
        assert_eq!(detector.get_tier(path), Some(ContextTier::Cold));
    }

    #[test]
    fn prune_cold_removes_only_cold() {
        let mut detector = DuplicateReadDetector::new();

        let hot_path = Path::new("/hot.rs");
        let warm_path = Path::new("/warm.rs");
        let cold_path = Path::new("/cold.rs");

        detector.register_read_with_tier(hot_path, ContextTier::Hot);
        detector.register_read_with_tier(warm_path, ContextTier::Warm);
        detector.register_read_with_tier(cold_path, ContextTier::Cold);

        let removed = detector.prune_cold();
        assert_eq!(removed, 1);
        assert!(detector.has_been_read(hot_path));
        assert!(detector.has_been_read(warm_path));
        assert!(!detector.has_been_read(cold_path));
    }

    #[test]
    fn clear_resets_all() {
        let mut detector = DuplicateReadDetector::new();
        let path = Path::new("/some/file.rs");

        detector.detect_duplicate_read(path);
        assert_eq!(detector.unique_file_count(), 1);

        detector.clear();
        assert_eq!(detector.unique_file_count(), 0);
        assert!(!detector.has_been_read(path));
    }

    #[test]
    fn tier_stats() {
        let mut detector = DuplicateReadDetector::new();

        detector.register_read_with_tier(Path::new("/a.rs"), ContextTier::Hot);
        detector.register_read_with_tier(Path::new("/b.rs"), ContextTier::Warm);
        detector.register_read_with_tier(Path::new("/c.rs"), ContextTier::Cold);

        // Read one file again
        detector.detect_duplicate_read(Path::new("/b.rs"));

        let stats = detector.tier_stats();
        assert_eq!(stats.hot_count, 1);
        assert_eq!(stats.warm_count, 1);
        assert_eq!(stats.cold_count, 1);
        assert_eq!(stats.total_reads, 4);
    }

    #[test]
    fn notice_context_string() {
        let notice = DuplicateNotice {
            original_read_id: 42,
            path: PathBuf::from("/src/main.rs"),
            timestamp: 1_234_567_890,
            read_count: 3,
            tier: ContextTier::Warm,
        };

        let s = notice.to_context_string();
        assert!(s.contains("main.rs"));
        assert!(s.contains("3 time"));
    }

    #[test]
    fn canonical_path_matching() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.rs");
        fs::write(&file_path, "content").unwrap();

        let mut detector = DuplicateReadDetector::new();

        // Read with relative-ish path
        detector.detect_duplicate_read(&file_path);

        // Read same file with canonical path
        let canonical = file_path.canonicalize().unwrap();
        let result = detector.detect_duplicate_read(&canonical);

        assert!(result.is_some()); // Should be detected as duplicate
    }

    #[test]
    fn context_tier_priority() {
        assert!(ContextTier::Hot.priority() > ContextTier::Warm.priority());
        assert!(ContextTier::Warm.priority() > ContextTier::Cold.priority());
    }
}
