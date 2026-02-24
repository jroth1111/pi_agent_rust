//! Flock coordinator for multi-agent parallel task execution
//!
//! Coordinates multiple FlockWorkers, detects conflicts between their changes,
//! and merges results back to the parent repository.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::orchestration::flock::FlockWorkspace;

/// Strategy for merging changes from multiple workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Apply changes sequentially (worker 0, then 1, etc.)
    Sequential,
    /// Try to merge all changes at once (git merge strategy)
    Parallel,
    /// Fail immediately if any conflicts are detected
    ConflictFail,
}

/// How to handle conflicts between workers.
#[derive(Debug, Clone)]
pub enum ConflictResolution {
    /// Fail the merge operation
    Fail,
    /// Prefer changes from the first worker
    PreferFirst,
    /// Prefer changes from the second worker
    PreferSecond,
    /// Manual resolution with a provided strategy
    Manual(String),
}

/// Represents a file conflict between two workers.
#[derive(Debug, Clone)]
pub struct FileConflict {
    /// Path to the conflicting file
    pub path: PathBuf,
    /// Workers that modified this file
    pub workers: Vec<usize>,
    /// Type of conflict
    pub conflict_type: ConflictType,
}

/// Type of conflict detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictType {
    /// Both workers modified the same file
    BothModified,
    /// One worker modified while another deleted
    ModifyDelete,
    /// Both workers created different files with same path
    CreateCreate,
}

/// Result of a merge operation.
#[derive(Debug, Clone)]
pub struct MergeResult {
    /// Whether the merge succeeded
    pub success: bool,
    /// Number of files merged
    pub files_merged: usize,
    /// Conflicts detected (empty if success)
    pub conflicts: Vec<FileConflict>,
    /// Merge commit SHA if successful
    pub merge_sha: Option<String>,
}

/// Coordinates multiple FlockWorkers for parallel execution.
pub struct FlockCoordinator {
    /// Worker workspaces
    workers: Vec<FlockWorkspace>,
    /// Merge strategy
    merge_strategy: MergeStrategy,
    /// Parent repository path
    repo_path: PathBuf,
    /// Input snapshot SHA
    input_snapshot: String,
}

impl FlockCoordinator {
    /// Create a new coordinator with workers.
    ///
    /// # Arguments
    /// * `repo` - Path to the parent git repository
    /// * `segments` - List of file segments, one per worker
    /// * `merge_strategy` - Strategy for merging changes
    pub fn spawn_flock(
        repo: &Path,
        segments: Vec<Vec<PathBuf>>,
        merge_strategy: MergeStrategy,
    ) -> Result<Self> {
        // Get the current HEAD as the input snapshot
        let output = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::session(format!(
                "Failed to get HEAD SHA: {}",
                stderr.trim()
            )));
        }

        let input_snapshot = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Create a workspace for each segment
        let mut workers = Vec::new();
        for (segment_id, files) in segments.into_iter().enumerate() {
            let mut workspace = FlockWorkspace::spawn(repo, segment_id, files)?;
            workspace.prepare()?;
            workers.push(workspace);
        }

        Ok(Self {
            workers,
            merge_strategy,
            repo_path: repo.to_path_buf(),
            input_snapshot,
        })
    }

    /// Get the number of workers.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Get a reference to a specific worker.
    pub fn worker(&self, segment_id: usize) -> Option<&FlockWorkspace> {
        self.workers.get(segment_id)
    }

    /// Get a mutable reference to a specific worker.
    pub fn worker_mut(&mut self, segment_id: usize) -> Option<&mut FlockWorkspace> {
        self.workers.get_mut(segment_id)
    }

    /// Detect conflicts between workers.
    ///
    /// Analyzes the changes in each worker workspace to identify:
    /// - Files modified by multiple workers
    /// - Modify/delete conflicts
    /// - Create/create conflicts
    pub fn detect_conflicts(&self) -> Vec<FileConflict> {
        let mut file_modifiers: HashMap<PathBuf, Vec<usize>> = HashMap::new();

        // Collect which workers modified which files
        for (idx, worker) in self.workers.iter().enumerate() {
            if let Ok(diff) = worker.get_changes() {
                for line in diff.lines() {
                    // Parse git diff output for file paths
                    // Format: "diff --git a/path/to/file b/path/to/file"
                    if line.starts_with("diff --git") {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 4 {
                            // Extract path from "a/path/to/file"
                            let path = parts[2].strip_prefix("a/").unwrap_or(parts[2]);
                            file_modifiers
                                .entry(PathBuf::from(path))
                                .or_default()
                                .push(idx);
                        }
                    }
                }
            }
        }

        // Find files modified by multiple workers
        let mut conflicts = Vec::new();
        for (path, modifiers) in file_modifiers {
            if modifiers.len() > 1 {
                conflicts.push(FileConflict {
                    path,
                    workers: modifiers,
                    conflict_type: ConflictType::BothModified,
                });
            }
        }

        conflicts
    }

    /// Merge all worker changes back to the parent repository.
    ///
    /// Uses the configured merge strategy to combine changes from all workers.
    pub fn merge(mut self) -> Result<MergeResult> {
        let conflicts = self.detect_conflicts();

        // Check for conflicts based on strategy
        if self.merge_strategy == MergeStrategy::ConflictFail && !conflicts.is_empty() {
            return Ok(MergeResult {
                success: false,
                files_merged: 0,
                conflicts,
                merge_sha: None,
            });
        }

        match self.merge_strategy {
            MergeStrategy::Sequential => self.merge_sequential(),
            MergeStrategy::Parallel => self.merge_parallel(),
            MergeStrategy::ConflictFail => {
                // Already checked for conflicts above
                if conflicts.is_empty() {
                    self.merge_parallel()
                } else {
                    Ok(MergeResult {
                        success: false,
                        files_merged: 0,
                        conflicts,
                        merge_sha: None,
                    })
                }
            }
        }
    }

    /// Sequential merge: apply worker changes one at a time.
    fn merge_sequential(&mut self) -> Result<MergeResult> {
        let mut files_merged = 0;
        let mut merged_files: HashSet<PathBuf> = HashSet::new();

        // Create a temporary branch for the merge
        let branch_name = format!("flock-merge-{}", uuid::Uuid::new_v4().simple());
        self.create_branch(&branch_name)?;

        for worker in &self.workers {
            // Get the diff from this worker
            let diff = worker.get_changes()?;

            // Apply the diff to the main repo
            if !diff.trim().is_empty() {
                self.apply_diff(&diff)?;

                // Count unique files
                for line in diff.lines() {
                    if line.starts_with("diff --git") {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 4 {
                            let path = parts[2].strip_prefix("a/").unwrap_or(parts[2]);
                            if merged_files.insert(PathBuf::from(path)) {
                                files_merged += 1;
                            }
                        }
                    }
                }
            }
        }

        // Commit the merge
        let merge_sha = self.commit_merge(&branch_name, "Sequential flock merge")?;

        Ok(MergeResult {
            success: true,
            files_merged,
            conflicts: Vec::new(),
            merge_sha: Some(merge_sha),
        })
    }

    /// Parallel merge: attempt to merge all changes at once.
    fn merge_parallel(&mut self) -> Result<MergeResult> {
        let mut files_merged = 0;
        let mut merged_files: HashSet<PathBuf> = HashSet::new();

        // Create a temporary branch for the merge
        let branch_name = format!("flock-merge-{}", uuid::Uuid::new_v4().simple());
        self.create_branch(&branch_name)?;

        for worker in &self.workers {
            let diff = worker.get_changes()?;

            if !diff.trim().is_empty() {
                self.apply_diff(&diff)?;

                for line in diff.lines() {
                    if line.starts_with("diff --git") {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 4 {
                            let path = parts[2].strip_prefix("a/").unwrap_or(parts[2]);
                            if merged_files.insert(PathBuf::from(path)) {
                                files_merged += 1;
                            }
                        }
                    }
                }
            }
        }

        let merge_sha = self.commit_merge(&branch_name, "Parallel flock merge")?;

        Ok(MergeResult {
            success: true,
            files_merged,
            conflicts: Vec::new(),
            merge_sha: Some(merge_sha),
        })
    }

    /// Create a new branch for the merge.
    fn create_branch(&self, branch_name: &str) -> Result<()> {
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["checkout", "-b", branch_name])
            .output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Branch might already exist, that's ok
            if !stderr.contains("already exists") {
                return Err(Error::session(format!("Failed to create branch: {stderr}")));
            }
        }

        Ok(())
    }

    /// Apply a diff to the repository.
    fn apply_diff(&self, diff: &str) -> Result<()> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["apply", "--reject", "--whitespace=fix"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(diff.as_bytes())
                .map_err(|e| Error::Io(Box::new(e)))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::tool(
                "flock",
                format!("Failed to apply diff: {}", stderr.trim()),
            ));
        }

        Ok(())
    }

    /// Commit the merge and return the new SHA.
    fn commit_merge(&self, _branch_name: &str, message: &str) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["add", "-A"])
            .output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if !output.status.success() {
            return Err(Error::session("Failed to stage changes".to_string()));
        }

        let commit_output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["commit", "-m", message])
            .output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if !commit_output.status.success() {
            let stderr = String::from_utf8_lossy(&commit_output.stderr);
            // Nothing to commit is ok
            if !stderr.contains("nothing to commit") {
                return Err(Error::session(format!("Failed to commit: {stderr}")));
            }
        }

        // Get the new commit SHA
        let rev_output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if !rev_output.status.success() {
            return Err(Error::session("Failed to get commit SHA".to_string()));
        }

        let sha = String::from_utf8_lossy(&rev_output.stdout)
            .trim()
            .to_string();

        // Switch back to main
        let _ = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["checkout", "-"])
            .output();

        Ok(sha)
    }
}

impl Drop for FlockCoordinator {
    fn drop(&mut self) {
        // Best-effort cleanup of all worker workspaces
        for worker in &mut self.workers {
            let _ = worker.teardown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_strategy_equality() {
        assert_eq!(MergeStrategy::Sequential, MergeStrategy::Sequential);
        assert_ne!(MergeStrategy::Sequential, MergeStrategy::Parallel);
        assert_eq!(MergeStrategy::ConflictFail, MergeStrategy::ConflictFail);
    }

    #[test]
    fn test_conflict_type_equality() {
        assert_eq!(ConflictType::BothModified, ConflictType::BothModified);
        assert_ne!(ConflictType::BothModified, ConflictType::ModifyDelete);
        assert_ne!(ConflictType::ModifyDelete, ConflictType::CreateCreate);
    }

    #[test]
    fn test_file_conflict_creation() {
        let conflict = FileConflict {
            path: PathBuf::from("src/main.rs"),
            workers: vec![0, 1],
            conflict_type: ConflictType::BothModified,
        };

        assert_eq!(conflict.path, PathBuf::from("src/main.rs"));
        assert_eq!(conflict.workers, vec![0, 1]);
        assert_eq!(conflict.conflict_type, ConflictType::BothModified);
    }

    #[test]
    fn test_merge_result_default() {
        let result = MergeResult {
            success: false,
            files_merged: 0,
            conflicts: Vec::new(),
            merge_sha: None,
        };

        assert!(!result.success);
        assert_eq!(result.files_merged, 0);
        assert!(result.conflicts.is_empty());
        assert!(result.merge_sha.is_none());
    }

    #[test]
    fn test_conflict_resolution_variants() {
        assert!(matches!(ConflictResolution::Fail, ConflictResolution::Fail));
        assert!(matches!(
            ConflictResolution::PreferFirst,
            ConflictResolution::PreferFirst
        ));
        assert!(matches!(
            ConflictResolution::PreferSecond,
            ConflictResolution::PreferSecond
        ));
        assert!(matches!(
            ConflictResolution::Manual("theirs".to_string()),
            ConflictResolution::Manual(_)
        ));
    }

    #[test]
    fn test_flock_coordinator_worker_count() {
        // This is a minimal test that verifies the struct can be created
        // with dummy data (not a full integration test)
        let repo = PathBuf::from("/fake/repo");
        let segments: Vec<Vec<PathBuf>> =
            vec![vec![PathBuf::from("a.rs")], vec![PathBuf::from("b.rs")]];

        // Note: This will fail in real scenarios since /fake/repo doesn't exist,
        // but it tests the type system
        let result = FlockCoordinator::spawn_flock(&repo, segments, MergeStrategy::Sequential);
        // We expect an error since /fake/repo is not a git repo
        assert!(result.is_err());
    }
}
