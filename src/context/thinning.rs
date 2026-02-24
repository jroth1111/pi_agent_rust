//! g3-style output thinning for large tool outputs.
//!
//! This module implements output thinning to reduce context window usage
//! from large tool outputs. When outputs exceed a configurable threshold,
//! they are written to disk and replaced with a file pointer in context.
//!
//! # Example
//!
//! ```
//! use pi::context::thinning::{OutputThinner, ThinnedOutput};
//! use std::path::Path;
//!
//! let thinner = OutputThinner::new();
//!
//! let large_output = "x".repeat(20_000); // 20KB
//! let result = thinner.thin_to_disk(&large_output, None);
//!
//! match result {
//!     ThinnedOutput::Inline(content) => println!("Kept inline: {} bytes", content.len()),
//!     ThinnedOutput::FilePointer { path, size } => {
//!         println!("Thinned to file: {:?} ({} bytes)", path, size);
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default threshold for thinning (10KB).
pub const DEFAULT_THRESHOLD_BYTES: usize = 10 * 1024;

/// Maximum age in seconds for thinned files before cleanup (24 hours).
pub const MAX_FILE_AGE_SECS: u64 = 24 * 60 * 60;

/// Prefix for thinned files.
pub const THINNED_FILE_PREFIX: &str = "pi_thinned_";

/// Output after thinning has been applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ThinnedOutput {
    /// Content was small enough to keep inline.
    Inline(String),
    /// Content was thinned to a file.
    FilePointer {
        /// Path to the thinned file.
        path: PathBuf,
        /// Original size in bytes.
        original_size: usize,
    },
}

impl ThinnedOutput {
    /// Check if this output was thinned.
    pub const fn is_thinned(&self) -> bool {
        matches!(self, Self::FilePointer { .. })
    }

    /// Get the content, loading from disk if thinned.
    ///
    /// # Errors
    ///
    /// Returns an error if the file pointer cannot be read.
    pub fn content(&self) -> crate::error::Result<String> {
        match self {
            Self::Inline(s) => Ok(s.clone()),
            Self::FilePointer { path, .. } => {
                fs::read_to_string(path).map_err(|e| crate::error::Error::Io(Box::new(e)))
            }
        }
    }

    /// Format a pointer message for context inclusion.
    pub fn to_context_string(&self) -> String {
        match self {
            Self::Inline(s) => s.clone(),
            Self::FilePointer {
                path,
                original_size,
            } => {
                format!(
                    "[Output thinned to file: {} ({} bytes)]",
                    path.display(),
                    original_size
                )
            }
        }
    }
}

/// Configuration for the output thinner.
#[derive(Debug, Clone)]
pub struct ThinnerConfig {
    /// Threshold in bytes above which outputs are thinned.
    pub threshold_bytes: usize,
    /// Directory to store thinned files (None uses default).
    pub storage_dir: Option<PathBuf>,
    /// Whether to compress thinned files.
    pub compress: bool,
    /// Maximum age in seconds for thinned files.
    pub max_age_secs: u64,
}

impl Default for ThinnerConfig {
    fn default() -> Self {
        Self {
            threshold_bytes: DEFAULT_THRESHOLD_BYTES,
            storage_dir: None,
            compress: false,
            max_age_secs: MAX_FILE_AGE_SECS,
        }
    }
}

/// g3-style output thinner for large tool outputs.
///
/// When tool outputs exceed a configurable threshold, this thinner
/// writes them to disk and returns a pointer instead, reducing
/// context window usage.
#[derive(Debug, Clone)]
pub struct OutputThinner {
    config: ThinnerConfig,
    /// Counter for unique file names.
    file_counter: u64,
}

impl Default for OutputThinner {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputThinner {
    /// Create a new output thinner with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: ThinnerConfig::default(),
            file_counter: 0,
        }
    }

    /// Create a thinner with custom configuration.
    #[must_use]
    pub const fn with_config(config: ThinnerConfig) -> Self {
        Self {
            config,
            file_counter: 0,
        }
    }

    /// Create a thinner with a custom threshold.
    #[must_use]
    pub fn with_threshold(threshold_bytes: usize) -> Self {
        Self {
            config: ThinnerConfig {
                threshold_bytes,
                ..ThinnerConfig::default()
            },
            file_counter: 0,
        }
    }

    /// Get the current threshold in bytes.
    pub const fn threshold(&self) -> usize {
        self.config.threshold_bytes
    }

    /// Set the threshold in bytes.
    pub const fn set_threshold(&mut self, bytes: usize) {
        self.config.threshold_bytes = bytes;
    }

    /// Thin output to disk if it exceeds the threshold.
    ///
    /// If the content is smaller than the threshold, returns it inline.
    /// Otherwise, writes to disk and returns a file pointer.
    ///
    /// # Arguments
    /// * `content` - The content to potentially thin
    /// * `artifact_dir` - Optional directory for thinned files (uses default if None)
    ///
    /// # Errors
    ///
    /// Returns an error if writing to disk fails.
    pub fn thin_to_disk(&mut self, content: &str, artifact_dir: Option<&Path>) -> ThinnedOutput {
        if content.len() <= self.config.threshold_bytes {
            return ThinnedOutput::Inline(content.to_string());
        }

        let storage_dir = self.get_storage_dir(artifact_dir);

        // Ensure directory exists
        if let Err(e) = fs::create_dir_all(&storage_dir) {
            tracing::warn!("Failed to create storage directory: {}", e);
            return ThinnedOutput::Inline(content.to_string());
        }

        // Generate unique filename
        let timestamp = current_timestamp();
        self.file_counter += 1;
        let filename = format!(
            "{}{}_{}.txt",
            THINNED_FILE_PREFIX, timestamp, self.file_counter
        );
        let file_path = storage_dir.join(filename);

        // Write content to file
        match self.write_content(&file_path, content) {
            Ok(()) => ThinnedOutput::FilePointer {
                path: file_path,
                original_size: content.len(),
            },
            Err(e) => {
                tracing::warn!("Failed to write thinned file: {}", e);
                ThinnedOutput::Inline(content.to_string())
            }
        }
    }

    /// Write content to file, optionally compressing.
    fn write_content(&self, path: &Path, content: &str) -> crate::error::Result<()> {
        let mut file = fs::File::create(path).map_err(|e| crate::error::Error::Io(Box::new(e)))?;

        if self.config.compress {
            // For now, just write uncompressed (compression would require additional deps)
            file.write_all(content.as_bytes())
                .map_err(|e| crate::error::Error::Io(Box::new(e)))?;
        } else {
            file.write_all(content.as_bytes())
                .map_err(|e| crate::error::Error::Io(Box::new(e)))?;
        }

        Ok(())
    }

    /// Get the storage directory, creating a default if needed.
    fn get_storage_dir(&self, artifact_dir: Option<&Path>) -> PathBuf {
        if let Some(dir) = artifact_dir {
            dir.join("thinned")
        } else if let Some(dir) = &self.config.storage_dir {
            dir.clone()
        } else {
            // Default to ~/tmp/
            dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("pi")
                .join("thinned")
        }
    }

    /// Clean up old thinned files.
    ///
    /// Removes files older than `max_age_secs` in the storage directory.
    ///
    /// # Arguments
    /// * `storage_dir` - Directory to clean (None uses default)
    ///
    /// # Returns
    /// Number of files removed.
    pub fn cleanup_old_files(&self, storage_dir: Option<&Path>) -> usize {
        let dir = self.get_storage_dir(storage_dir);
        let now = current_timestamp();
        let mut removed = 0;

        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                    if filename.starts_with(THINNED_FILE_PREFIX) {
                        // Check file age
                        if let Ok(metadata) = entry.metadata() {
                            if let Ok(modified) = metadata.modified() {
                                if let Ok(age) = modified.duration_since(UNIX_EPOCH) {
                                    let file_time = age.as_secs();
                                    if now.saturating_sub(file_time) > self.config.max_age_secs
                                        && fs::remove_file(&path).is_ok()
                                    {
                                        removed += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        removed
    }

    /// Remove a specific thinned file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be removed.
    pub fn remove_thinned_file(&self, path: &Path) -> crate::error::Result<()> {
        fs::remove_file(path).map_err(|e| crate::error::Error::Io(Box::new(e)))
    }

    /// Get statistics about thinned files in storage.
    ///
    /// # Arguments
    /// * `storage_dir` - Directory to scan (None uses default)
    ///
    /// # Returns
    /// Tuple of (file count, total bytes on disk).
    pub fn storage_stats(&self, storage_dir: Option<&Path>) -> (usize, u64) {
        let dir = self.get_storage_dir(storage_dir);
        let mut count = 0;
        let mut total_bytes = 0u64;

        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                    if filename.starts_with(THINNED_FILE_PREFIX) {
                        if let Ok(metadata) = entry.metadata() {
                            count += 1;
                            total_bytes += metadata.len();
                        }
                    }
                }
            }
        }

        (count, total_bytes)
    }

    /// Clear all thinned files from storage.
    ///
    /// # Arguments
    /// * `storage_dir` - Directory to clear (None uses default)
    ///
    /// # Returns
    /// Number of files removed.
    pub fn clear_storage(&self, storage_dir: Option<&Path>) -> usize {
        let dir = self.get_storage_dir(storage_dir);
        let mut removed = 0;

        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                    if filename.starts_with(THINNED_FILE_PREFIX) && fs::remove_file(&path).is_ok() {
                        removed += 1;
                    }
                }
            }
        }

        removed
    }
}

/// Get the current timestamp in seconds since Unix epoch.
fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn small_output_inline() {
        let mut thinner = OutputThinner::new();
        let content = "hello world";
        let result = thinner.thin_to_disk(content, None);

        assert!(matches!(result, ThinnedOutput::Inline(_)));
        assert!(!result.is_thinned());
    }

    #[test]
    fn large_output_thinned() {
        let mut thinner = OutputThinner::with_threshold(100);
        let content = "x".repeat(200);
        let result = thinner.thin_to_disk(&content, None);

        assert!(matches!(result, ThinnedOutput::FilePointer { .. }));
        assert!(result.is_thinned());
    }

    #[test]
    fn thinned_content_can_be_loaded() {
        let temp_dir = TempDir::new().unwrap();
        let mut thinner = OutputThinner::with_threshold(100);
        let content = "x".repeat(200);

        let result = thinner.thin_to_disk(&content, Some(temp_dir.path()));
        let loaded = result.content().unwrap();

        assert_eq!(loaded, content);
    }

    #[test]
    fn context_string_for_inline() {
        let result = ThinnedOutput::Inline("test".to_string());
        let s = result.to_context_string();
        assert_eq!(s, "test");
    }

    #[test]
    fn context_string_for_thinned() {
        let result = ThinnedOutput::FilePointer {
            path: PathBuf::from("/tmp/test.txt"),
            original_size: 1000,
        };
        let s = result.to_context_string();
        assert!(s.contains("thinned to file"));
        assert!(s.contains("1000 bytes"));
    }

    #[test]
    fn cleanup_removes_old_files() {
        let temp_dir = TempDir::new().unwrap();

        // Create a thinner that cleans up files older than 0 seconds
        let config = ThinnerConfig {
            threshold_bytes: 100,
            storage_dir: Some(temp_dir.path().join("thinned")),
            max_age_secs: 0,
            compress: false,
        };
        let mut thinner = OutputThinner::with_config(config);

        // Create a thinned file
        let content = "x".repeat(200);
        let _ = thinner.thin_to_disk(&content, None);

        // Wait 1 second for the file to be "older than 0 seconds"
        // (timestamp comparison is in seconds, so sub-second waits won't work)
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Cleanup should remove the file
        let removed = thinner.cleanup_old_files(None);
        assert!(removed >= 1);
    }

    #[test]
    fn storage_stats() {
        let temp_dir = TempDir::new().unwrap();
        let mut thinner = OutputThinner::with_config(ThinnerConfig {
            threshold_bytes: 100,
            storage_dir: Some(temp_dir.path().join("thinned")),
            max_age_secs: MAX_FILE_AGE_SECS,
            compress: false,
        });

        // No files yet
        let (count, _) = thinner.storage_stats(None);
        assert_eq!(count, 0);

        // Create some thinned files
        let content = "x".repeat(200);
        thinner.thin_to_disk(&content, None);
        thinner.thin_to_disk(&content, None);

        let (count, bytes) = thinner.storage_stats(None);
        assert_eq!(count, 2);
        assert!(bytes > 0);
    }

    #[test]
    fn clear_storage() {
        let temp_dir = TempDir::new().unwrap();
        let mut thinner = OutputThinner::with_config(ThinnerConfig {
            threshold_bytes: 100,
            storage_dir: Some(temp_dir.path().join("thinned")),
            max_age_secs: MAX_FILE_AGE_SECS,
            compress: false,
        });

        // Create some thinned files
        let content = "x".repeat(200);
        thinner.thin_to_disk(&content, None);
        thinner.thin_to_disk(&content, None);

        let (count, _) = thinner.storage_stats(None);
        assert_eq!(count, 2);

        // Clear all
        let removed = thinner.clear_storage(None);
        assert_eq!(removed, 2);

        let (count, _) = thinner.storage_stats(None);
        assert_eq!(count, 0);
    }

    #[test]
    fn threshold_getter_setter() {
        let mut thinner = OutputThinner::new();
        assert_eq!(thinner.threshold(), DEFAULT_THRESHOLD_BYTES);

        thinner.set_threshold(5000);
        assert_eq!(thinner.threshold(), 5000);
    }

    #[test]
    fn exact_threshold_stays_inline() {
        let mut thinner = OutputThinner::with_threshold(100);
        let content = "x".repeat(100);

        let result = thinner.thin_to_disk(&content, None);
        assert!(matches!(result, ThinnedOutput::Inline(_)));
    }

    #[test]
    fn just_over_threshold_thinned() {
        let mut thinner = OutputThinner::with_threshold(100);
        let content = "x".repeat(101);

        let result = thinner.thin_to_disk(&content, None);
        assert!(matches!(result, ThinnedOutput::FilePointer { .. }));
    }
}
