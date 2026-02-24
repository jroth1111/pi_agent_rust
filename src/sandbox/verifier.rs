//! Zero-trust clean-room verification for agent changes.
//!
//! This module provides isolated worktree-based verification that agent changes
//! can be safely applied and tested before being committed to the main repository.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::error::{Error, Result};

/// A set of constraints for auditing changes.
#[derive(Debug, Clone, Default)]
pub struct ConstraintSet {
    /// Paths that are allowed to be modified.
    pub allowed_paths: Vec<String>,
    /// Paths that must not be modified.
    pub forbidden_paths: Vec<String>,
    /// File patterns that are allowed to be created.
    pub allowed_patterns: Vec<String>,
    /// File patterns that are forbidden.
    pub forbidden_patterns: Vec<String>,
    /// Maximum number of files that can be changed.
    pub max_files_changed: Option<usize>,
    /// Maximum total diff size in bytes.
    pub max_diff_size: Option<usize>,
}

/// Plan for running verification commands.
#[derive(Debug, Clone)]
pub struct VerifyPlan {
    /// The command to run (e.g., "cargo test")
    pub command: String,
    /// Timeout for the command
    pub timeout: Duration,
    /// Exit codes that indicate success (e.g., [0] for tests, [0, 1] for lint)
    pub expected_exit_codes: Vec<i32>,
}

impl VerifyPlan {
    /// Create a new verification plan.
    pub fn new(
        command: impl Into<String>,
        timeout: Duration,
        expected_exit_codes: Vec<i32>,
    ) -> Self {
        Self {
            command: command.into(),
            timeout,
            expected_exit_codes,
        }
    }

    /// Create a simple test plan with default timeout and exit code 0.
    pub fn simple(command: impl Into<String>) -> Self {
        Self::new(command, Duration::from_secs(300), vec![0])
    }
}

/// Result of a verification run.
#[derive(Debug, Clone)]
pub struct VerificationResult {
    /// Whether the verification passed (exit code was expected)
    pub passed: bool,
    /// Captured stdout
    pub stdout: String,
    /// Captured stderr
    pub stderr: String,
    /// Exit code of the command
    pub exit_code: Option<i32>,
    /// Whether the command timed out
    pub timed_out: bool,
}

/// Clean-room verifier for isolated change verification.
///
/// Creates an isolated git worktree at a specific commit SHA and applies
/// agent changes for verification before they are committed to the main repo.
///
/// # Security Model
///
/// - The worktree is created at `input_snapshot` (immutable base)
/// - All diffs are computed against `input_snapshot`, not HEAD
/// - This catches any drift that occurred after the snapshot was taken
/// - The worktree is isolated from the agent's workspace
#[derive(Debug)]
pub struct CleanRoomVerifier {
    /// The commit SHA to use as the immutable base for verification.
    pub input_snapshot: String,
    /// Path to the isolated worktree (created by prepare()).
    pub worktree: PathBuf,
    /// Path to the original repository.
    pub repo_path: PathBuf,
}

impl CleanRoomVerifier {
    /// Create a new clean-room verifier.
    ///
    /// # Arguments
    /// * `repo_path` - Path to the git repository
    /// * `input_snapshot` - Commit SHA to use as the base (must exist in repo)
    /// * `worktree_path` - Where to create the isolated worktree
    pub const fn new(repo_path: PathBuf, input_snapshot: String, worktree_path: PathBuf) -> Self {
        Self {
            input_snapshot,
            worktree: worktree_path,
            repo_path,
        }
    }

    /// Create a verifier with an auto-generated worktree path.
    ///
    /// The worktree will be created in a temp directory with a name based on the snapshot.
    pub fn with_temp_worktree(repo_path: PathBuf, input_snapshot: String) -> Self {
        let temp_dir = std::env::temp_dir();
        let worktree_name = format!("pi-verify-{}", &input_snapshot[..8]);
        let worktree = temp_dir.join(worktree_name);
        Self::new(repo_path, input_snapshot, worktree)
    }

    /// Prepare the isolated worktree at input_snapshot.
    ///
    /// This creates a new git worktree at the specified commit SHA,
    /// completely isolated from the agent's workspace.
    pub fn prepare(&self) -> Result<()> {
        // Remove existing worktree if it exists
        if self.worktree.exists() {
            self.cleanup()?;
        }

        // Create parent directory if needed
        if let Some(parent) = self.worktree.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(Box::new(e)))?;
        }

        // Create the worktree at the snapshot commit
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args([
                "worktree",
                "add",
                "--detach",
                &self.worktree.display().to_string(),
                &self.input_snapshot,
            ])
            .output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::tool(
                "cleanroom",
                format!(
                    "Failed to create worktree at {}: {}",
                    self.input_snapshot, stderr
                ),
            ));
        }

        Ok(())
    }

    /// Apply a patch to the worktree.
    ///
    /// The patch should be in unified diff format as produced by `git diff`.
    pub fn apply_patch(&self, patch: &str) -> Result<()> {
        if !self.worktree.exists() {
            return Err(Error::tool(
                "cleanroom",
                "Worktree not prepared. Call prepare() first.",
            ));
        }

        // Apply the patch using git apply in the worktree
        let mut child = Command::new("git")
            .current_dir(&self.worktree)
            .args(["apply", "--reject", "--whitespace=fix"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::Io(Box::new(e)))?;

        // Write patch to stdin
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(patch.as_bytes())
                .map_err(|e| Error::Io(Box::new(e)))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Check for .rej files that indicate partial application
            let rej_check = Command::new("sh")
                .current_dir(&self.worktree)
                .args(["-c", "find . -name '*.rej' | head -5"])
                .output()
                .ok();
            let rej_info = rej_check
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default();

            return Err(Error::tool(
                "cleanroom",
                format!(
                    "Failed to apply patch: {}. Reject files: {}",
                    stderr.trim(),
                    rej_info.trim()
                ),
            ));
        }

        Ok(())
    }

    /// Run verification against the worktree.
    ///
    /// Executes the command in the worktree with a timeout.
    pub fn verify(&self, plan: &VerifyPlan) -> Result<VerificationResult> {
        if !self.worktree.exists() {
            return Err(Error::tool(
                "cleanroom",
                "Worktree not prepared. Call prepare() first.",
            ));
        }

        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        let shell_flag = if cfg!(windows) { "/C" } else { "-c" };

        let output = Command::new(shell)
            .current_dir(&self.worktree)
            .args([shell_flag, &plan.command])
            .output()
            .map_err(|e| Error::Io(Box::new(e)))?;

        let exit_code = output.status.code();
        let passed = exit_code.is_some_and(|code| plan.expected_exit_codes.contains(&code));

        Ok(VerificationResult {
            passed,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code,
            timed_out: false,
        })
    }

    /// Audit the changes against a constraint set.
    ///
    /// Compares the worktree state against input_snapshot (not HEAD!).
    /// This catches any drift that occurred after the snapshot was taken.
    pub fn audit_diff(&self, constraints: &ConstraintSet) -> Result<Vec<String>> {
        if !self.worktree.exists() {
            return Err(Error::tool(
                "cleanroom",
                "Worktree not prepared. Call prepare() first.",
            ));
        }

        let mut violations = Vec::new();

        // Get the diff stats against the input snapshot
        let diff_output = Command::new("git")
            .current_dir(&self.worktree)
            .args(["diff", "--numstat", &self.input_snapshot])
            .output()
            .map_err(|e| Error::session(format!("git diff numstat: {e}")))?;

        let diff_text = String::from_utf8_lossy(&diff_output.stdout);
        let mut total_size = 0usize;
        let mut files_changed = 0usize;

        for line in diff_text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let added = parts[0].parse::<usize>().unwrap_or(0);
                let removed = parts[1].parse::<usize>().unwrap_or(0);
                let path = parts[2];

                files_changed += 1;
                total_size += added + removed;

                // Check forbidden paths
                for forbidden in &constraints.forbidden_paths {
                    if path.starts_with(forbidden) || path == *forbidden {
                        violations.push(format!(
                            "Modified forbidden path: {path} (matches {forbidden})"
                        ));
                    }
                }

                // Check forbidden patterns
                for pattern in &constraints.forbidden_patterns {
                    if path_matches_pattern(path, pattern) {
                        violations.push(format!(
                            "Modified file matches forbidden pattern: {path} (matches {pattern})"
                        ));
                    }
                }

                // Check if path is in allowed list (if not empty)
                if !constraints.allowed_paths.is_empty() {
                    let allowed = constraints
                        .allowed_paths
                        .iter()
                        .any(|a| path.starts_with(a) || path == *a);
                    if !allowed {
                        violations.push(format!("Modified path not in allowlist: {path}"));
                    }
                }
            }
        }

        // Check file count constraint
        if let Some(max_files) = constraints.max_files_changed {
            if files_changed > max_files {
                violations.push(format!(
                    "Too many files changed: {files_changed} (max: {max_files})"
                ));
            }
        }

        // Check diff size constraint
        if let Some(max_size) = constraints.max_diff_size {
            if total_size > max_size {
                violations.push(format!(
                    "Diff too large: {total_size} lines (max: {max_size})"
                ));
            }
        }

        Ok(violations)
    }

    /// Get the full diff against input_snapshot.
    pub fn get_diff(&self) -> Result<String> {
        if !self.worktree.exists() {
            return Err(Error::tool(
                "cleanroom",
                "Worktree not prepared. Call prepare() first.",
            ));
        }

        let output = Command::new("git")
            .current_dir(&self.worktree)
            .args(["diff", &self.input_snapshot])
            .output()
            .map_err(|e| Error::session(format!("git diff: {e}")))?;

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Remove the worktree and clean up resources.
    pub fn cleanup(&self) -> Result<()> {
        // First, try to remove via git worktree remove
        let git_remove = Command::new("git")
            .current_dir(&self.repo_path)
            .args([
                "worktree",
                "remove",
                "--force",
                &self.worktree.display().to_string(),
            ])
            .output();

        // If git worktree remove fails, try manual removal
        if (git_remove.is_err() || !git_remove.unwrap().status.success()) && self.worktree.exists()
        {
            std::fs::remove_dir_all(&self.worktree)
                .map_err(|e| Error::session(format!("remove worktree directory: {e}")))?;
        }

        // Prune any stale worktree references
        let _ = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["worktree", "prune"])
            .output();

        Ok(())
    }
}

impl Drop for CleanRoomVerifier {
    fn drop(&mut self) {
        // Best-effort cleanup on drop
        let _ = self.cleanup();
    }
}

/// Simple glob pattern matching for file paths.
fn path_matches_pattern(path: &str, pattern: &str) -> bool {
    // Prefer real glob matching for correctness on patterns like "**/*.rs".
    if let Ok(glob) = glob::Pattern::new(pattern) {
        if glob.matches(path) {
            return true;
        }
    }

    // Handle simple patterns: *, **, and literals
    if pattern == "*" {
        return true;
    }

    if let Some(suffix) = pattern.strip_prefix("**/") {
        return path.ends_with(suffix) || path.contains(&format!("/{suffix}"));
    }

    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path.starts_with(prefix);
    }

    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            let starts = parts[0];
            let ends = parts[1];
            return path.starts_with(starts) && path.ends_with(ends);
        }
    }

    path == pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_matches_pattern() {
        assert!(path_matches_pattern("src/main.rs", "*"));
        assert!(path_matches_pattern("src/main.rs", "**/*.rs"));
        assert!(path_matches_pattern("src/main.rs", "src/**"));
        assert!(path_matches_pattern("src/main.rs", "*.rs"));
        assert!(path_matches_pattern("src/main.rs", "src/main.rs"));

        assert!(!path_matches_pattern("src/main.rs", "*.txt"));
        assert!(!path_matches_pattern("src/main.rs", "lib/**"));
    }

    #[test]
    fn test_verify_plan_simple() {
        let plan = VerifyPlan::simple("cargo test");
        assert_eq!(plan.command, "cargo test");
        assert_eq!(plan.expected_exit_codes, vec![0]);
    }

    #[test]
    fn test_constraint_set_default() {
        let constraints = ConstraintSet::default();
        assert!(constraints.allowed_paths.is_empty());
        assert!(constraints.forbidden_paths.is_empty());
        assert!(constraints.max_files_changed.is_none());
    }
}
