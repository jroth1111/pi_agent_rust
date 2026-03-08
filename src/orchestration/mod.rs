//! Multi-agent orchestration for parallel task execution
//!
//! This module provides workspace isolation and coordination for multiple
//! agents working in parallel (a "Flock"). It prevents semantic stomping by
//! creating isolated worktrees per agent and coordinating changes.

pub mod coordinator;
pub mod flock;
pub mod run;

#[cfg(test)]
mod tests;

pub use coordinator::{
    ConflictResolution, FileConflict, FlockCoordinator, MergeResult, MergeStrategy,
};
pub use flock::{FlockWorker, FlockWorkspace};
pub use run::{
    ExecutionTier, RunLifecycle, RunStatus, RunStore, RunVerifyScopeKind, RunVerifyStatus,
    SubrunPlan, TaskReport, WaveStatus,
};
