//! Flock worker and isolated workspace management
//!
//! Provides per-worker isolation using git worktrees, allowing multiple
//! agents to work on different file segments without semantic stomping.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use crate::error::{Error, Result};
use crate::sandbox::verifier::CleanRoomVerifier;

/// Worker in a Flock (parallel agent group)
///
/// Each worker is assigned a segment of files and works in an isolated
/// workspace to prevent interference with other workers.
#[derive(Debug, Clone)]
pub struct FlockWorker {
    /// Segment identifier (0-based index)
    pub segment_id: usize,
    /// Cloned workspace path (e.g., `segment-0/`)
    pub workspace: PathBuf,
    /// Immutable input snapshot SHA (base commit for this worker)
    pub input_snapshot: String,
    /// Files assigned to this worker
    pub assigned_files: Vec<PathBuf>,
}

impl FlockWorker {
    /// Create a new flock worker descriptor.
    ///
    /// # Arguments
    /// * `segment_id` - Unique identifier for this worker
    /// * `workspace` - Path where the isolated workspace will be created
    /// * `input_snapshot` - Git SHA to use as the immutable base
    /// * `assigned_files` - Files this worker is responsible for
    pub const fn new(
        segment_id: usize,
        workspace: PathBuf,
        input_snapshot: String,
        assigned_files: Vec<PathBuf>,
    ) -> Self {
        Self {
            segment_id,
            workspace,
            input_snapshot,
            assigned_files,
        }
    }

    /// Get the path for an assigned file within the workspace.
    pub fn workspace_file_path(&self, file: &Path) -> PathBuf {
        self.workspace.join(file)
    }
}

/// Isolated workspace for a FlockWorker
///
/// Manages the lifecycle of an isolated git worktree for a single worker.
pub struct FlockWorkspace {
    /// Worker descriptor
    pub worker: FlockWorker,
    /// Underlying clean-room verifier for worktree management
    verifier: CleanRoomVerifier,
    /// Whether the workspace has been prepared
    prepared: bool,
}

impl FlockWorkspace {
    fn derive_workspace_path(parent_repo: &Path, segment_id: usize) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        parent_repo.to_string_lossy().hash(&mut hasher);
        let repo_hash = hasher.finish();
        std::env::temp_dir().join(format!("pi-flock-{repo_hash:x}-segment-{segment_id}"))
    }

    /// Create isolated workspace from parent repo.
    ///
    /// # Arguments
    /// * `parent_repo` - Path to the parent git repository
    /// * `segment_id` - Unique identifier for this worker
    /// * `files` - Files assigned to this worker
    ///
    /// # Returns
    /// A new `FlockWorkspace` that must be `prepare()`'d before use.
    pub fn spawn(parent_repo: &Path, segment_id: usize, files: Vec<PathBuf>) -> Result<Self> {
        // Get the current HEAD as the input snapshot
        let output = Command::new("git")
            .current_dir(parent_repo)
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

        // Include repo fingerprint in the workspace path to avoid cross-repo/test collisions.
        let workspace = Self::derive_workspace_path(parent_repo, segment_id);

        let verifier = CleanRoomVerifier::new(
            parent_repo.to_path_buf(),
            input_snapshot.clone(),
            workspace.clone(),
        );
        let worker = FlockWorker::new(segment_id, workspace, input_snapshot, files);

        Ok(Self {
            worker,
            verifier,
            prepared: false,
        })
    }

    /// Create a workspace with a custom input snapshot.
    ///
    /// Use this when you need to base the workspace on a specific commit
    /// rather than the current HEAD.
    pub fn spawn_with_snapshot(
        parent_repo: &Path,
        segment_id: usize,
        files: Vec<PathBuf>,
        input_snapshot: String,
    ) -> Result<Self> {
        let workspace = Self::derive_workspace_path(parent_repo, segment_id);

        let verifier = CleanRoomVerifier::new(
            parent_repo.to_path_buf(),
            input_snapshot.clone(),
            workspace.clone(),
        );
        let worker = FlockWorker::new(segment_id, workspace, input_snapshot, files);

        Ok(Self {
            worker,
            verifier,
            prepared: false,
        })
    }

    /// Prepare the isolated workspace.
    ///
    /// Creates the git worktree at the input snapshot. Must be called
    /// before `get_changes()` or any file operations.
    pub fn prepare(&mut self) -> Result<()> {
        self.verifier.prepare()?;
        self.prepared = true;
        Ok(())
    }

    /// Get the diff against input_snapshot.
    ///
    /// Returns the unified diff of all changes made in this workspace
    /// compared to the immutable base snapshot.
    pub fn get_changes(&self) -> Result<String> {
        if !self.prepared {
            return Err(Error::tool(
                "flock",
                "Workspace not prepared. Call prepare() first.",
            ));
        }
        self.verifier.get_diff()
    }

    /// Get the diff against the current worktree HEAD.
    ///
    /// Useful when the workspace has already been layered with prior patches and
    /// the caller only wants the incremental delta from the latest committed base.
    pub fn get_changes_since_head(&self) -> Result<String> {
        if !self.prepared {
            return Err(Error::tool(
                "flock",
                "Workspace not prepared. Call prepare() first.",
            ));
        }
        self.verifier.get_diff_from_head()
    }

    /// Apply a patch inside the prepared workspace.
    pub fn apply_patch(&self, patch: &str) -> Result<()> {
        if !self.prepared {
            return Err(Error::tool(
                "flock",
                "Workspace not prepared. Call prepare() first.",
            ));
        }
        self.verifier.apply_patch(patch)
    }

    /// Commit the current staged workspace state to create a new internal base.
    pub fn commit_staged_changes(&self, message: &str) -> Result<()> {
        if !self.prepared {
            return Err(Error::tool(
                "flock",
                "Workspace not prepared. Call prepare() first.",
            ));
        }
        self.verifier.commit_staged_changes(message)
    }

    /// Get the path to this workspace.
    pub fn workspace_path(&self) -> &Path {
        &self.worker.workspace
    }

    /// Get the worker's segment ID.
    pub const fn segment_id(&self) -> usize {
        self.worker.segment_id
    }

    /// Get the files assigned to this worker.
    pub fn assigned_files(&self) -> &[PathBuf] {
        &self.worker.assigned_files
    }

    /// Get the input snapshot SHA.
    pub fn input_snapshot(&self) -> &str {
        &self.worker.input_snapshot
    }

    /// Check if this workspace has been prepared.
    pub const fn is_prepared(&self) -> bool {
        self.prepared
    }

    /// Cleanup the workspace.
    ///
    /// Removes the git worktree and all temporary files.
    pub fn teardown(&mut self) -> Result<()> {
        self.verifier.cleanup()?;
        self.prepared = false;
        Ok(())
    }
}

impl Drop for FlockWorkspace {
    fn drop(&mut self) {
        // Best-effort cleanup on drop
        if self.prepared {
            let _ = self.verifier.cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flock_worker_creation() {
        let workspace = PathBuf::from("/tmp/test-segment-0");
        let files = vec![PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")];
        let worker = FlockWorker::new(0, workspace, "abc123".to_string(), files);

        assert_eq!(worker.segment_id, 0);
        assert_eq!(worker.input_snapshot, "abc123");
        assert_eq!(worker.assigned_files.len(), 2);
    }

    #[test]
    fn test_flock_workspace_file_path() {
        let workspace = PathBuf::from("/tmp/test-segment-0");
        let files = vec![PathBuf::from("src/main.rs")];
        let worker = FlockWorker::new(0, workspace, "abc123".to_string(), files);

        let main_path = worker.workspace_file_path(Path::new("src/main.rs"));
        assert!(main_path.ends_with("src/main.rs"));
    }

    #[test]
    fn test_flock_workspace_accessors() {
        let workspace = PathBuf::from("/tmp/test-segment-1");
        let files = vec![PathBuf::from("test.rs")];
        let worker = FlockWorker::new(1, workspace.clone(), "def456".to_string(), files.clone());

        assert_eq!(worker.segment_id, 1);
        assert_eq!(worker.workspace, workspace);
        assert_eq!(worker.input_snapshot, "def456");
        assert_eq!(worker.assigned_files, files);
    }
}
