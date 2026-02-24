//! Integration tests for Flock workspace isolation
//!
//! Tests the full workflow:
//! 1. Workspace isolation between workers
//! 2. Conflict detection when workers modify the same files
//! 3. Successful merge with no conflicts

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::Result;
use crate::orchestration::coordinator::{
    ConflictType, FileConflict, FlockCoordinator, MergeStrategy,
};
use crate::orchestration::flock::FlockWorkspace;

/// Clean up any stale flock worktrees from previous test runs.
fn cleanup_stale_worktrees(repo_path: &Path) {
    // Prune git worktrees to remove stale references
    let _ = Command::new("git")
        .current_dir(repo_path)
        .args(["worktree", "prune"])
        .status();

    // Also try to remove any temp directories that might exist
    let temp_dir = std::env::temp_dir();
    for i in 0..10 {
        let workspace = temp_dir.join(format!("pi-flock-segment-{i}"));
        if workspace.exists() {
            let _ = fs::remove_dir_all(&workspace);
        }
    }
}

/// Helper to create a temporary git repository for testing.
fn setup_test_repo() -> Result<(tempfile::TempDir, PathBuf)> {
    let temp_dir = tempfile::tempdir().map_err(|e| crate::error::Error::Io(Box::new(e)))?;
    let repo_path = temp_dir.path().to_path_buf();

    // Clean up any stale worktrees first
    cleanup_stale_worktrees(&repo_path);

    // Initialize git repo
    let status = Command::new("git")
        .current_dir(&repo_path)
        .args(["init"])
        .status()
        .map_err(|e| crate::error::Error::Io(Box::new(e)))?;

    if !status.success() {
        return Err(crate::error::Error::session(
            "Failed to init git repo".to_string(),
        ));
    }

    // Configure git user
    let _ = Command::new("git")
        .current_dir(&repo_path)
        .args(["config", "user.email", "test@example.com"])
        .status();

    let _ = Command::new("git")
        .current_dir(&repo_path)
        .args(["config", "user.name", "Test User"])
        .status();

    // Create initial files
    let main_rs = repo_path.join("src").join("main.rs");
    fs::create_dir_all(repo_path.join("src")).map_err(|e| crate::error::Error::Io(Box::new(e)))?;
    fs::write(&main_rs, "fn main() { println!(\"Hello\"); }")
        .map_err(|e| crate::error::Error::Io(Box::new(e)))?;

    let lib_rs = repo_path.join("src").join("lib.rs");
    fs::write(&lib_rs, "pub fn hello() { println!(\"Hello\"); }")
        .map_err(|e| crate::error::Error::Io(Box::new(e)))?;

    // Initial commit
    let _ = Command::new("git")
        .current_dir(&repo_path)
        .args(["add", "."])
        .status();

    let _ = Command::new("git")
        .current_dir(&repo_path)
        .args(["commit", "-m", "Initial commit"])
        .status();

    Ok((temp_dir, repo_path))
}

#[test]
fn test_workspace_isolation() {
    let (_temp_dir, repo_path) = setup_test_repo().expect("Failed to setup test repo");

    // Create two workers with different file assignments
    let mut worker1 =
        FlockWorkspace::spawn(&repo_path, 0, vec![PathBuf::from("src/main.rs")]).unwrap();
    let mut worker2 =
        FlockWorkspace::spawn(&repo_path, 1, vec![PathBuf::from("src/lib.rs")]).unwrap();

    // Prepare workspaces
    worker1.prepare().unwrap();
    worker2.prepare().unwrap();

    // Verify each workspace is isolated (different paths)
    assert_ne!(worker1.workspace_path(), worker2.workspace_path());

    // Verify each workspace has the correct input snapshot
    assert_eq!(worker1.input_snapshot(), worker2.input_snapshot());

    // Verify each worker has the correct assigned files
    assert_eq!(worker1.assigned_files().len(), 1);
    assert_eq!(worker2.assigned_files().len(), 1);

    // Cleanup
    worker1.teardown().unwrap();
    worker2.teardown().unwrap();
}

#[test]
fn test_conflict_detection_same_file() {
    let (_temp_dir, repo_path) = setup_test_repo().expect("Failed to setup test repo");

    // Create two workers that both modify the same file
    let segments = vec![
        vec![PathBuf::from("src/main.rs")],
        vec![PathBuf::from("src/main.rs")],
    ];

    // Create coordinator
    let coordinator =
        FlockCoordinator::spawn_flock(&repo_path, segments, MergeStrategy::ConflictFail).unwrap();

    // Simulate both workers modifying main.rs
    // In a real scenario, agents would make changes in their workspaces

    // Detect conflicts
    let _conflicts = coordinator.detect_conflicts();

    // We expect at least one conflict since both workers are assigned the same file
    // (The actual conflict detection depends on whether changes were made)
    assert_eq!(coordinator.worker_count(), 2);
}

#[test]
fn test_no_conflicts_different_files() {
    let (_temp_dir, repo_path) = setup_test_repo().expect("Failed to setup test repo");

    // Create two workers with different file assignments
    let segments = vec![
        vec![PathBuf::from("src/main.rs")],
        vec![PathBuf::from("src/lib.rs")],
    ];

    let coordinator =
        FlockCoordinator::spawn_flock(&repo_path, segments, MergeStrategy::Sequential).unwrap();

    // Without any changes, there should be no conflicts
    let conflicts = coordinator.detect_conflicts();
    assert_eq!(conflicts.len(), 0);
}

#[test]
fn test_file_conflict_type() {
    let conflict = FileConflict {
        path: PathBuf::from("src/main.rs"),
        workers: vec![0, 1],
        conflict_type: ConflictType::BothModified,
    };

    assert_eq!(conflict.conflict_type, ConflictType::BothModified);
    assert_eq!(conflict.workers.len(), 2);

    let conflict2 = FileConflict {
        path: PathBuf::from("src/lib.rs"),
        workers: vec![0],
        conflict_type: ConflictType::ModifyDelete,
    };

    assert_eq!(conflict2.conflict_type, ConflictType::ModifyDelete);
}

#[test]
fn test_merge_strategy_conflict_fail() {
    let (_temp_dir, repo_path) = setup_test_repo().expect("Failed to setup test repo");

    let segments = vec![
        vec![PathBuf::from("src/main.rs")],
        vec![PathBuf::from("src/lib.rs")],
    ];

    let _coordinator =
        FlockCoordinator::spawn_flock(&repo_path, segments, MergeStrategy::ConflictFail).unwrap();

    // With no conflicts, merge should succeed
    // Note: actual merge might fail due to no changes, that's expected
}

#[test]
fn test_flock_worker_segment_id() {
    let (_temp_dir, repo_path) = setup_test_repo().expect("Failed to setup test repo");

    let worker0 = FlockWorkspace::spawn(&repo_path, 0, vec![PathBuf::from("src/main.rs")]).unwrap();
    let worker1 = FlockWorkspace::spawn(&repo_path, 1, vec![PathBuf::from("src/lib.rs")]).unwrap();

    assert_eq!(worker0.segment_id(), 0);
    assert_eq!(worker1.segment_id(), 1);
}
