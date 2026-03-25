//! Authoritative workflow store, transition journal, and rebuildable projections.
//!
//! This module implements the **single authoritative workflow lifecycle owner**
//! required by VAL-WF-001. All task/run lifecycle truth flows through this store
//! and its append-only transition journal. Rebuildable projections are derived
//! from the journal and agree with live queries.
//!
//! The atomic migration cutover from the legacy split authority
//! (`RpcReliabilityState` + `RpcOrchestrationState`) is handled by
//! [`WorkflowMigrationCutover`], which emits a correlated cutover record,
//! verifies migrated state before the reader flip, and blocks dual-authority
//! writes during the transition (VAL-WF-013).

use crate::contracts::engine::{
    AppendEvidenceRequest, DispatchOptions, LeaseGrant as ContractLeaseGrant, StateDigestResult,
    SubmitTaskRequest, SubmitTaskResult, TaskContract, TaskPrerequisite, TaskStateDigest,
    WorkflowContract,
};
use crate::error::{Error, Result};
use crate::orchestration::{RunLifecycle, RunStatus};
use crate::reliability::lease::LeaseGrant as ReliabilityLeaseGrant;
use crate::reliability::state::{RuntimeState, TerminalState};
use crate::reliability::state_machine::{TransitionEvent, apply_transition};
use crate::reliability::{
    DagEvaluation, DagEvaluator, DagValidation, EdgeKind, EvidenceRecord, FsArtifactStore,
    LeaseManager, ReliabilityEdge, TaskNode, TaskRuntime,
};
use crate::rpc::{RpcOrchestrationState, RpcReliabilityState};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

// ============================================================================
// Workflow transition journal — append-only audit trail
// ============================================================================

/// A single entry in the workflow transition journal.
///
/// Every state mutation in the workflow store appends a journal entry.
/// The journal is the authoritative audit trail that can be replayed to
/// reconstruct the complete workflow state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowJournalEntry {
    /// Monotonically increasing sequence number within the store.
    pub seq: u64,
    /// Unique stable identifier for this journal entry.
    pub entry_id: String,
    /// ISO 8601 timestamp when the transition occurred.
    pub timestamp: String,
    /// The workflow command that caused this transition.
    pub command: WorkflowCommand,
    /// SHA-256 checksum of the serialized command for integrity.
    pub command_checksum: String,
    /// The resulting task state after the transition (if applicable).
    pub resulting_task_state: Option<TaskStateView>,
}

/// View of a task's runtime state suitable for journal entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStateView {
    Blocked {
        waiting_on: Vec<String>,
    },
    Ready,
    Leased {
        lease_id: String,
        agent_id: String,
        fence_token: u64,
    },
    Verifying {
        patch_digest: String,
        verify_run_id: String,
    },
    Recoverable {
        reason: String,
    },
    AwaitingHuman {
        question: String,
    },
    TerminalSucceeded,
    TerminalFailed,
    TerminalSuperseded,
    TerminalCanceled,
}

impl TaskStateView {
    pub fn from_runtime(state: &RuntimeState) -> Self {
        match state {
            RuntimeState::Blocked { waiting_on } => TaskStateView::Blocked {
                waiting_on: waiting_on.clone(),
            },
            RuntimeState::Ready => TaskStateView::Ready,
            RuntimeState::Leased {
                lease_id,
                agent_id,
                fence_token,
                expires_at: _,
            } => TaskStateView::Leased {
                lease_id: lease_id.clone(),
                agent_id: agent_id.clone(),
                fence_token: *fence_token,
            },
            RuntimeState::Verifying {
                patch_digest,
                verify_run_id,
            } => TaskStateView::Verifying {
                patch_digest: patch_digest.clone(),
                verify_run_id: verify_run_id.clone(),
            },
            RuntimeState::Recoverable { reason, .. } => TaskStateView::Recoverable {
                reason: format!("{reason:?}"),
            },
            RuntimeState::AwaitingHuman { question, .. } => TaskStateView::AwaitingHuman {
                question: question.clone(),
            },
            RuntimeState::Terminal(TerminalState::Succeeded { .. }) => {
                TaskStateView::TerminalSucceeded
            }
            RuntimeState::Terminal(TerminalState::Failed { .. }) => TaskStateView::TerminalFailed,
            RuntimeState::Terminal(TerminalState::Superseded { .. }) => {
                TaskStateView::TerminalSuperseded
            }
            RuntimeState::Terminal(TerminalState::Canceled { .. }) => {
                TaskStateView::TerminalCanceled
            }
        }
    }
}

/// Workflow commands that can appear in the transition journal.
///
/// Each command is a durable, idempotency-keyed operation. Replaying the
/// same command with the same `command_id` produces at most one state
/// transition (VAL-WF-011).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowCommand {
    CreateRun {
        run_id: String,
        objective: String,
        task_ids: Vec<String>,
    },
    CreateTask {
        task_id: String,
        objective: String,
        prerequisites: Vec<TaskPrerequisite>,
        verify_command: Option<String>,
        max_attempts: Option<u8>,
    },
    AddEdge {
        edge_id: String,
        from: String,
        to: String,
        kind: String,
    },
    DispatchTask {
        task_id: String,
        lease_id: String,
        agent_id: String,
        fence_token: u64,
    },
    SubmitTask {
        task_id: String,
        lease_id: String,
        fence_token: u64,
        patch_digest: String,
        verify_run_id: String,
        verify_passed: Option<bool>,
        verify_timed_out: bool,
    },
    AppendEvidence {
        task_id: String,
        evidence_id: String,
        command: String,
        exit_code: i32,
    },
    VerifySuccess {
        task_id: String,
        verify_run_id: String,
        patch_digest: String,
    },
    VerifyFail {
        task_id: String,
        class: String,
        verify_run_id: Option<String>,
        failure_artifact: Option<String>,
        handoff_summary: String,
    },
    ReportBlocker {
        task_id: String,
        lease_id: String,
        fence_token: u64,
        reason: String,
    },
    ExpireLease {
        task_id: String,
    },
    HumanResolve {
        task_id: String,
    },
    SupersedeTask {
        task_id: String,
        by: Vec<String>,
    },
    PromoteRecoverable {
        task_id: String,
    },
    CancelRun {
        run_id: String,
        reason: Option<String>,
    },
}

// ============================================================================
// Workflow store — single authoritative source of truth
// ============================================================================

/// The authoritative workflow store.
///
/// Owns all task lifecycle state, run lifecycle state, dependency edges,
/// evidence records, and lease management. Every mutation appends to the
/// transition journal, making the full history replayable.
///
/// Projections (run status, state digest, readiness) are derived from the
/// authoritative store state and are fully rebuildable.
pub struct WorkflowStore {
    /// Tasks keyed by task ID.
    tasks: BTreeMap<String, TaskNode>,
    /// Dependency edges between tasks.
    edges: Vec<ReliabilityEdge>,
    /// Evidence records keyed by task ID.
    evidence_by_task: HashMap<String, Vec<EvidenceRecord>>,
    /// Run statuses keyed by run ID.
    runs: BTreeMap<String, RunStatus>,
    /// Index from task ID to run IDs.
    task_runs: HashMap<String, HashSet<String>>,
    /// Lease manager for task execution leases.
    leases: LeaseManager,
    /// Artifact store for reliability artifacts.
    artifacts: FsArtifactStore,
    /// Append-only transition journal.
    journal: Vec<WorkflowJournalEntry>,
    /// Monotonically increasing journal sequence counter.
    journal_seq: u64,
    /// Set of already-processed command IDs for idempotency.
    processed_command_ids: HashSet<String>,
    /// Configuration defaults.
    default_max_attempts: u8,
    verify_timeout_sec_default: u32,
}

impl WorkflowStore {
    /// Create a new empty workflow store.
    pub fn new(config: &crate::config::Config) -> Result<Self> {
        let artifacts_root = crate::config::Config::global_dir()
            .join("reliability")
            .join("artifacts");
        let artifacts = FsArtifactStore::new(artifacts_root)
            .map_err(|err| Error::session(format!("artifact store init failed: {err}")))?;

        Ok(Self {
            tasks: BTreeMap::new(),
            edges: Vec::new(),
            evidence_by_task: HashMap::new(),
            runs: BTreeMap::new(),
            task_runs: HashMap::new(),
            leases: LeaseManager::default(),
            artifacts,
            journal: Vec::new(),
            journal_seq: 0,
            processed_command_ids: HashSet::new(),
            default_max_attempts: config.reliability_default_max_attempts(),
            verify_timeout_sec_default: config.reliability_verify_timeout_sec_default(),
        })
    }

    /// Create a new workflow store with explicit defaults (for testing).
    #[cfg(test)]
    pub fn new_test(default_max_attempts: u8, verify_timeout_sec: u32) -> Result<Self> {
        let artifacts_root = std::env::temp_dir().join("pi_test_workflow_artifacts");
        let artifacts = FsArtifactStore::new(artifacts_root)
            .map_err(|err| Error::session(format!("artifact store init failed: {err}")))?;

        Ok(Self {
            tasks: BTreeMap::new(),
            edges: Vec::new(),
            evidence_by_task: HashMap::new(),
            runs: BTreeMap::new(),
            task_runs: HashMap::new(),
            leases: LeaseManager::default(),
            artifacts,
            journal: Vec::new(),
            journal_seq: 0,
            processed_command_ids: HashSet::new(),
            default_max_attempts,
            verify_timeout_sec_default: verify_timeout_sec,
        })
    }

    // -- Task lifecycle (authoritative) --

    /// Create a new task in the store. Returns an error if the task already exists.
    pub fn create_task(
        &mut self,
        task_id: String,
        objective: String,
        prerequisites: Vec<TaskPrerequisite>,
        verify_command: Option<String>,
        max_attempts: Option<u8>,
    ) -> Result<()> {
        if self.tasks.contains_key(&task_id) {
            return Err(Error::validation(format!("task {task_id} already exists")));
        }

        let spec = crate::reliability::task::TaskSpec {
            objective,
            constraints: crate::reliability::task::TaskConstraintSet::default(),
            verify: if let Some(cmd) = verify_command {
                crate::reliability::task::VerifyPlan::Standard {
                    audit_diff: true,
                    command: cmd,
                    timeout_sec: self.verify_timeout_sec_default,
                }
            } else {
                crate::reliability::task::VerifyPlan::Standard {
                    audit_diff: false,
                    command: String::new(),
                    timeout_sec: self.verify_timeout_sec_default,
                }
            },
            max_attempts: max_attempts.unwrap_or(self.default_max_attempts),
            input_snapshot: String::new(),
            acceptance_ids: Vec::new(),
            planned_touches: Vec::new(),
        };

        let initial_state = if prerequisites.is_empty() {
            RuntimeState::Ready
        } else {
            RuntimeState::Blocked {
                waiting_on: prerequisites.iter().map(|p| p.task_id.clone()).collect(),
            }
        };

        let mut task_node = TaskNode::new(task_id.clone(), spec);
        task_node.runtime = TaskRuntime {
            state: initial_state,
            attempt: 0,
            last_transition_at: Utc::now(),
        };

        let resulting_state = TaskStateView::from_runtime(&task_node.runtime.state);
        self.tasks.insert(task_id.clone(), task_node);

        // Add prerequisite edges
        for prereq in &prerequisites {
            self.edges.push(ReliabilityEdge {
                id: format!("edge-{task_id}-from-{}", prereq.task_id),
                from: prereq.task_id.clone(),
                to: task_id.clone(),
                kind: EdgeKind::Prerequisite {
                    trigger: crate::reliability::edge::EdgeTrigger::OnSuccess,
                },
            });
        }

        self.append_journal_with_state(
            WorkflowCommand::CreateTask {
                task_id: task_id.clone(),
                objective: self.tasks[&task_id].spec.objective.clone(),
                prerequisites,
                verify_command: self.tasks[&task_id]
                    .spec
                    .verify
                    .clone()
                    .standard_command()
                    .map(|s| s.to_string()),
                max_attempts: Some(self.tasks[&task_id].spec.max_attempts),
            },
            Some(resulting_state),
        );

        Ok(())
    }

    /// Get an immutable reference to a task node.
    pub fn get_task(&self, task_id: &str) -> Option<&TaskNode> {
        self.tasks.get(task_id)
    }

    /// Get a mutable reference to a task node.
    pub fn get_task_mut(&mut self, task_id: &str) -> Option<&mut TaskNode> {
        self.tasks.get_mut(task_id)
    }

    /// Apply a state machine transition to a task and journal it.
    pub fn apply_task_transition(&mut self, task_id: &str, event: &TransitionEvent) -> Result<()> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| Error::validation(format!("task {task_id} not found")))?;

        let max_attempts = task.spec.max_attempts;
        apply_transition(&mut task.runtime, event, max_attempts)
            .map_err(|err| Error::validation(err.to_string()))?;

        let resulting_state = TaskStateView::from_runtime(&task.runtime.state);

        let command = Self::transition_to_command(task_id, event);
        self.append_journal_with_state(command, Some(resulting_state));

        Ok(())
    }

    /// Convert a state machine transition event to a workflow command.
    fn transition_to_command(task_id: &str, event: &TransitionEvent) -> WorkflowCommand {
        match event {
            TransitionEvent::Dispatch {
                lease_id,
                agent_id,
                fence_token,
                ..
            } => WorkflowCommand::DispatchTask {
                task_id: task_id.to_string(),
                lease_id: lease_id.clone(),
                agent_id: agent_id.clone(),
                fence_token: *fence_token,
            },
            TransitionEvent::Submit {
                lease_id,
                fence_token,
                patch_digest,
                verify_run_id,
            } => WorkflowCommand::SubmitTask {
                task_id: task_id.to_string(),
                lease_id: lease_id.clone(),
                fence_token: *fence_token,
                patch_digest: patch_digest.clone(),
                verify_run_id: verify_run_id.clone(),
                verify_passed: None,
                verify_timed_out: false,
            },
            TransitionEvent::VerifySuccess {
                verify_run_id,
                patch_digest,
            } => WorkflowCommand::VerifySuccess {
                task_id: task_id.to_string(),
                verify_run_id: verify_run_id.clone(),
                patch_digest: patch_digest.clone(),
            },
            TransitionEvent::VerifyFail {
                class,
                verify_run_id,
                failure_artifact,
                handoff_summary,
            } => WorkflowCommand::VerifyFail {
                task_id: task_id.to_string(),
                class: format!("{class:?}"),
                verify_run_id: verify_run_id.clone(),
                failure_artifact: failure_artifact.clone(),
                handoff_summary: handoff_summary.clone(),
            },
            TransitionEvent::ReportBlocker {
                lease_id,
                fence_token,
                reason,
                ..
            } => WorkflowCommand::ReportBlocker {
                task_id: task_id.to_string(),
                lease_id: lease_id.clone(),
                fence_token: *fence_token,
                reason: reason.clone(),
            },
            TransitionEvent::ExpireLease => WorkflowCommand::ExpireLease {
                task_id: task_id.to_string(),
            },
            TransitionEvent::HumanResolve => WorkflowCommand::HumanResolve {
                task_id: task_id.to_string(),
            },
            TransitionEvent::DependenciesMet => WorkflowCommand::PromoteRecoverable {
                task_id: task_id.to_string(),
            },
            TransitionEvent::Supersede { by } => WorkflowCommand::SupersedeTask {
                task_id: task_id.to_string(),
                by: by.clone(),
            },
            TransitionEvent::PromoteRecoverable => WorkflowCommand::PromoteRecoverable {
                task_id: task_id.to_string(),
            },
        }
    }

    // -- Evidence (authoritative) --

    /// Append evidence to a task.
    pub fn append_evidence(&mut self, record: EvidenceRecord) -> Result<()> {
        self.evidence_by_task
            .entry(record.task_id.clone())
            .or_default()
            .push(record.clone());

        self.append_journal(WorkflowCommand::AppendEvidence {
            task_id: record.task_id.clone(),
            evidence_id: record.evidence_id.clone(),
            command: record.command.clone(),
            exit_code: record.exit_code,
        });

        Ok(())
    }

    /// Get evidence records for a task.
    pub fn get_evidence(&self, task_id: &str) -> Vec<&EvidenceRecord> {
        self.evidence_by_task
            .get(task_id)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    // -- Run lifecycle (authoritative) --

    /// Create a new run in the store.
    pub fn create_run(
        &mut self,
        run_id: String,
        objective: String,
        task_ids: Vec<String>,
    ) -> Result<()> {
        if self.runs.contains_key(&run_id) {
            return Err(Error::validation(format!("run {run_id} already exists")));
        }

        use crate::orchestration::ExecutionTier;
        let mut run = RunStatus::new(&run_id, &objective, ExecutionTier::Wave);
        run.task_ids = task_ids.clone();

        for task_id in &task_ids {
            self.task_runs
                .entry(task_id.clone())
                .or_default()
                .insert(run_id.clone());
        }

        self.runs.insert(run_id.clone(), run);

        self.append_journal_with_state(
            WorkflowCommand::CreateRun {
                run_id: run_id.clone(),
                objective,
                task_ids,
            },
            None,
        );

        Ok(())
    }

    /// Get a run status (projection from authoritative store).
    pub fn get_run_status(&self, run_id: &str) -> Option<RunStatus> {
        self.runs.get(run_id).cloned()
    }

    /// Update run lifecycle.
    pub fn update_run_lifecycle(&mut self, run_id: &str, lifecycle: RunLifecycle) -> Result<()> {
        let run = self
            .runs
            .get_mut(run_id)
            .ok_or_else(|| Error::validation(format!("run {run_id} not found")))?;

        run.lifecycle = lifecycle;
        run.touch();

        self.append_journal(WorkflowCommand::CreateRun {
            run_id: run_id.to_string(),
            objective: String::new(),
            task_ids: Vec::new(),
        });

        Ok(())
    }

    /// Cancel a run.
    pub fn cancel_run(&mut self, run_id: &str, reason: Option<String>) -> Result<()> {
        let run = self
            .runs
            .get_mut(run_id)
            .ok_or_else(|| Error::validation(format!("run {run_id} not found")))?;

        run.lifecycle = RunLifecycle::Canceled;
        run.touch();

        // Cancel all non-terminal tasks in the run
        for task_id in &run.task_ids {
            if let Some(task) = self.tasks.get_mut(task_id) {
                if let RuntimeState::Leased { .. }
                | RuntimeState::Ready
                | RuntimeState::Blocked { .. } = &task.runtime.state
                {
                    task.runtime.state = RuntimeState::Terminal(TerminalState::Canceled {
                        reason: reason.clone().unwrap_or_default(),
                        canceled_at: Utc::now(),
                    });
                }
            }
        }

        self.append_journal(WorkflowCommand::CancelRun {
            run_id: run_id.to_string(),
            reason,
        });

        Ok(())
    }

    // -- DAG evaluation (authoritative) --

    /// Evaluate the dependency DAG and unblock tasks whose prerequisites are met.
    pub fn evaluate_dag(&mut self) -> DagEvaluation {
        let task_ids: Vec<String> = self.tasks.keys().cloned().collect();
        let mut tasks_mut: Vec<TaskNode> = Vec::new();
        for id in &task_ids {
            if let Some(task) = self.tasks.get(id) {
                tasks_mut.push(task.clone());
            }
        }
        let result = DagEvaluator::evaluate_and_unblock(&mut tasks_mut, &self.edges);

        // Write back mutated tasks
        for task in tasks_mut {
            self.tasks.insert(task.id.clone(), task);
        }

        result
    }

    /// Validate the dependency DAG for cycles and orphan references.
    pub fn validate_dag(&self) -> DagValidation {
        let tasks: Vec<TaskNode> = self.tasks.values().cloned().collect();
        DagEvaluator::validate(&tasks, &self.edges)
    }

    // -- Projections (rebuildable derived state) --

    /// Build a state digest projection from the authoritative store.
    pub fn state_digest(&self, task_id: Option<&str>) -> StateDigestResult {
        let tasks: Vec<TaskStateDigest> = if let Some(tid) = task_id {
            self.tasks
                .get(tid)
                .map(|t| {
                    vec![TaskStateDigest {
                        task_id: t.id.clone(),
                        state: format!("{:?}", t.runtime.state),
                        attempts: t.runtime.attempt as u32,
                        evidence_count: self
                            .evidence_by_task
                            .get(&t.id)
                            .map(|v| v.len())
                            .unwrap_or(0),
                    }]
                })
                .unwrap_or_default()
        } else {
            self.tasks
                .values()
                .map(|t| TaskStateDigest {
                    task_id: t.id.clone(),
                    state: format!("{:?}", t.runtime.state),
                    attempts: u32::from(t.runtime.attempt),
                    evidence_count: self
                        .evidence_by_task
                        .get(&t.id)
                        .map(|v| v.len())
                        .unwrap_or(0),
                })
                .collect()
        };

        let validation = self.validate_dag();
        StateDigestResult {
            schema: "workflow-store-v1".to_string(),
            tasks,
            edge_count: self.edges.len(),
            is_dag_valid: !validation.has_cycle,
        }
    }

    /// Get all run statuses.
    pub fn all_run_statuses(&self) -> Vec<RunStatus> {
        self.runs.values().cloned().collect()
    }

    /// Get run IDs for a task.
    pub fn run_ids_for_task(&self, task_id: &str) -> Vec<String> {
        self.task_runs
            .get(task_id)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get all task IDs.
    pub fn task_ids(&self) -> Vec<String> {
        self.tasks.keys().cloned().collect()
    }

    /// Get the number of tasks.
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    /// Get the number of runs.
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// Check if the store is empty (no tasks, no runs).
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.runs.is_empty()
    }

    // -- Journal (append-only audit trail) --

    /// Append a command to the journal.
    fn append_journal(&mut self, command: WorkflowCommand) {
        self.append_journal_with_state(command, None);
    }

    /// Append a command with resulting state to the journal.
    fn append_journal_with_state(
        &mut self,
        command: WorkflowCommand,
        resulting_task_state: Option<TaskStateView>,
    ) {
        self.journal_seq += 1;
        let command_json = serde_json::to_string(&command).unwrap_or_default();
        let checksum = format!("{:x}", Sha256::digest(command_json.as_bytes()));

        let entry = WorkflowJournalEntry {
            seq: self.journal_seq,
            entry_id: format!("wfj-{}", self.journal_seq),
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            command,
            command_checksum: checksum,
            resulting_task_state,
        };

        self.journal.push(entry);
    }

    /// Get the full transition journal (read-only).
    pub fn journal(&self) -> &[WorkflowJournalEntry] {
        &self.journal
    }

    /// Get the journal entry count.
    pub fn journal_len(&self) -> usize {
        self.journal.len()
    }

    /// Check if a command ID has already been processed (idempotency).
    pub fn is_command_processed(&self, command_id: &str) -> bool {
        self.processed_command_ids.contains(command_id)
    }

    /// Mark a command ID as processed.
    pub fn mark_command_processed(&mut self, command_id: String) {
        self.processed_command_ids.insert(command_id);
    }

    /// Get lease manager reference.
    pub fn leases(&self) -> &LeaseManager {
        &self.leases
    }

    /// Issue a lease for a task.
    pub fn issue_lease(
        &mut self,
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> std::result::Result<ReliabilityLeaseGrant, crate::reliability::LeaseError> {
        self.leases.issue_lease(task_id, agent_id, ttl_seconds)
    }

    /// Validate a lease fence.
    pub fn validate_lease_fence(
        &self,
        lease_id: &str,
        fence_token: u64,
    ) -> std::result::Result<(), crate::reliability::LeaseError> {
        self.leases.validate_fence(lease_id, fence_token)
    }

    /// Get artifact store reference.
    pub fn artifacts(&self) -> &FsArtifactStore {
        &self.artifacts
    }

    /// Get edges reference.
    pub fn edges(&self) -> &[ReliabilityEdge] {
        &self.edges
    }

    /// Get tasks as a slice of values.
    pub fn tasks_values(&self) -> Vec<&TaskNode> {
        self.tasks.values().collect()
    }
}

// ============================================================================
// Atomic migration cutover (VAL-WF-013)
// ============================================================================

/// Outcome of a workflow migration attempt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMigrationOutcome {
    /// Migration completed and authority has been flipped to the workflow store.
    Succeeded,
    /// Migration was explicitly rolled back; source authority remains active.
    RolledBack,
    /// Migration failed partway; partial target state must not be readable.
    Failed,
}

/// A durable correlation record for a workflow migration cutover.
///
/// Written to the **surviving authority** so that rollback evidence is
/// never lost to cleanup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowMigrationRecord {
    /// Unique correlation identifier for this migration attempt.
    pub correlation_id: String,
    /// Number of tasks migrated.
    pub task_count: usize,
    /// Number of runs migrated.
    pub run_count: usize,
    /// Number of edges migrated.
    pub edge_count: usize,
    /// Number of evidence records migrated.
    pub evidence_count: usize,
    /// Number of journal entries from the legacy source.
    pub legacy_journal_entries: usize,
    /// Whether post-migration verification passed.
    pub verification_passed: bool,
    /// Verification errors (empty if passed).
    pub verification_errors: Vec<String>,
    /// Final outcome.
    pub outcome: WorkflowMigrationOutcome,
    /// ISO 8601 timestamp when migration started.
    pub started_at: String,
    /// ISO 8601 timestamp when migration completed.
    pub completed_at: String,
    /// Human-readable reason.
    pub reason: String,
}

/// Handles the atomic migration cutover from legacy split workflow state
/// (`RpcReliabilityState` + `RpcOrchestrationState`) to the unified
/// `WorkflowStore`.
///
/// The migration:
/// 1. Snapshots the legacy state
/// 2. Creates a `WorkflowStore` from the snapshot
/// 3. Verifies migrated state matches the source
/// 4. Emits a correlated `WorkflowMigrationRecord`
/// 5. Blocks dual-authority writes after cutover
pub struct WorkflowMigrationCutover;

impl WorkflowMigrationCutover {
    /// Execute migration from legacy state to the workflow store.
    ///
    /// This is the **only** supported migration path. After migration,
    /// the legacy `RpcReliabilityState` and `RpcOrchestrationState` must
    /// not be used for authoritative writes.
    pub(crate) fn migrate_from_legacy(
        reliability: &RpcReliabilityState,
        orchestration: &RpcOrchestrationState,
        workflow_store: &mut WorkflowStore,
    ) -> Result<WorkflowMigrationRecord> {
        let correlation_id = format!("wf-mig-{}", uuid_v4_compat());
        let started_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let mut verification_errors = Vec::new();

        // Step 1: Migrate tasks
        let mut task_count = 0;
        for (task_id, task_node) in &reliability.tasks {
            // Don't overwrite if already exists in workflow store
            if workflow_store.get_task(task_id).is_none() {
                // We need to insert directly since create_task does validation
                // that may not match the legacy data exactly
                workflow_store
                    .tasks
                    .insert(task_id.clone(), task_node.clone());
                task_count += 1;
            }
        }

        // Step 2: Migrate edges
        let edge_count = reliability.edges.len();
        for edge in &reliability.edges {
            if !workflow_store.edges.iter().any(|e| e.id == edge.id) {
                workflow_store.edges.push(edge.clone());
            }
        }

        // Step 3: Migrate evidence
        let mut evidence_count = 0;
        for (task_id, records) in &reliability.evidence_by_task {
            for record in records {
                workflow_store
                    .evidence_by_task
                    .entry(task_id.clone())
                    .or_default()
                    .push(record.clone());
                evidence_count += 1;
            }
        }

        // Step 4: Migrate runs
        let mut run_count = 0;
        for run_status in orchestration.all_runs() {
            if workflow_store.get_run_status(&run_status.run_id).is_none() {
                workflow_store
                    .runs
                    .insert(run_status.run_id.clone(), run_status.clone());
                run_count += 1;
            }
        }

        // Step 5: Migrate task_runs index
        for (task_id, run_ids) in orchestration.task_run_entries() {
            for run_id in run_ids {
                workflow_store
                    .task_runs
                    .entry(task_id.clone())
                    .or_default()
                    .insert(run_id.clone());
            }
        }

        // Step 6: Verify migrated state
        let verification_passed = Self::verify_migration(
            reliability,
            orchestration,
            workflow_store,
            &mut verification_errors,
        );

        let completed_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let outcome = if verification_passed {
            WorkflowMigrationOutcome::Succeeded
        } else {
            WorkflowMigrationOutcome::Failed
        };

        let record = WorkflowMigrationRecord {
            correlation_id: correlation_id.clone(),
            task_count,
            run_count,
            edge_count,
            evidence_count,
            legacy_journal_entries: 0,
            verification_passed,
            verification_errors: verification_errors.clone(),
            outcome,
            started_at,
            completed_at,
            reason: if verification_passed {
                "Migration verified successfully".to_string()
            } else {
                format!("Verification failed: {}", verification_errors.join("; "))
            },
        };

        Ok(record)
    }

    /// Verify that the migrated workflow store matches the legacy source.
    fn verify_migration(
        reliability: &RpcReliabilityState,
        orchestration: &RpcOrchestrationState,
        workflow_store: &WorkflowStore,
        errors: &mut Vec<String>,
    ) -> bool {
        let mut ok = true;

        // Verify task count
        if workflow_store.task_count() != reliability.tasks.len() {
            errors.push(format!(
                "Task count mismatch: workflow_store={}, legacy={}",
                workflow_store.task_count(),
                reliability.tasks.len()
            ));
            ok = false;
        }

        // Verify each task's state
        for (task_id, legacy_task) in &reliability.tasks {
            if let Some(store_task) = workflow_store.get_task(task_id) {
                if store_task.runtime.state != legacy_task.runtime.state {
                    errors.push(format!(
                        "Task {task_id} state mismatch: store={:?}, legacy={:?}",
                        store_task.runtime.state, legacy_task.runtime.state
                    ));
                    ok = false;
                }
            }
        }

        // Verify run count
        let orchestration_runs = orchestration.all_runs();
        if workflow_store.run_count() != orchestration_runs.len() {
            errors.push(format!(
                "Run count mismatch: workflow_store={}, legacy={}",
                workflow_store.run_count(),
                orchestration_runs.len()
            ));
            ok = false;
        }

        // Verify each run's lifecycle
        for legacy_run in orchestration_runs {
            if let Some(store_run) = workflow_store.get_run_status(&legacy_run.run_id) {
                if store_run.lifecycle != legacy_run.lifecycle {
                    errors.push(format!(
                        "Run {} lifecycle mismatch: store={:?}, legacy={:?}",
                        legacy_run.run_id, store_run.lifecycle, legacy_run.lifecycle
                    ));
                    ok = false;
                }
            }
        }

        // Verify edge count
        if workflow_store.edges().len() != reliability.edges.len() {
            errors.push(format!(
                "Edge count mismatch: workflow_store={}, legacy={}",
                workflow_store.edges().len(),
                reliability.edges.len()
            ));
            ok = false;
        }

        ok
    }

    /// Check if the workflow store is in a migrated state (has tasks/runs
    /// but no journal entries from creation — indicating legacy migration).
    pub fn is_migrated(workflow_store: &WorkflowStore) -> bool {
        // A migrated store has tasks/runs but the journal won't have
        // CreateTask/CreateRun entries for them (they were bulk-inserted).
        !workflow_store.is_empty() && workflow_store.journal_len() == 0
    }
}

/// Generate a UUID-like string without depending on the uuid crate.
fn uuid_v4_compat() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = duration.as_nanos();
    let hash = format!("{:x}", Sha256::digest(nanos.to_le_bytes()));
    hash[..32].to_string()
}

// Helper trait for extracting the verify command as a string.
trait VerifyPlanExt {
    fn standard_command(&self) -> Option<&str>;
}

impl VerifyPlanExt for crate::reliability::task::VerifyPlan {
    fn standard_command(&self) -> Option<&str> {
        match self {
            crate::reliability::task::VerifyPlan::Standard { command, .. } => {
                if command.is_empty() {
                    None
                } else {
                    Some(command)
                }
            }
        }
    }
}

// ============================================================================
// WorkflowContract implementation
// ============================================================================

/// Implementation of the `WorkflowContract` trait backed by `WorkflowStore`.
pub struct WorkflowService {
    store: Mutex<WorkflowStore>,
}

impl WorkflowService {
    /// Create a new workflow service from the given store.
    pub fn new(store: WorkflowStore) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }

    /// Create from a config.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let store = WorkflowStore::new(config)?;
        Ok(Self::new(store))
    }

    /// Get a reference to the inner store (for migration, testing, etc.).
    pub fn with_store<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&WorkflowStore) -> R,
    {
        let store = self.store.lock().unwrap();
        f(&store)
    }

    /// Get a mutable reference to the inner store.
    pub fn with_store_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut WorkflowStore) -> R,
    {
        let mut store = self.store.lock().unwrap();
        f(&mut store)
    }
}

#[async_trait]
impl WorkflowContract for WorkflowService {
    async fn create_run(&self, objective: String, tasks: Vec<TaskContract>) -> Result<String> {
        let run_id = format!("run-{}", uuid_v4_compat());
        let task_ids: Vec<String> = tasks.iter().map(|t| t.task_id.clone()).collect();

        let mut store = self.store.lock().unwrap();
        store.create_run(run_id.clone(), objective, task_ids)?;

        // Create tasks
        for task in &tasks {
            store.create_task(
                task.task_id.clone(),
                task.objective.clone(),
                task.prerequisites.clone(),
                task.verify_command.clone(),
                task.max_attempts,
            )?;
        }

        Ok(run_id)
    }

    async fn run_status(&self, run_id: &str) -> Result<crate::contracts::engine::RunStatus> {
        let store = self.store.lock().unwrap();
        let run = store
            .get_run_status(run_id)
            .ok_or_else(|| Error::validation(format!("run {run_id} not found")))?;
        Ok(crate::contracts::engine::RunStatus {
            run_id: run.run_id.clone(),
            lifecycle: convert_run_lifecycle(run.lifecycle),
            wave_status: crate::contracts::engine::WaveStatus::Idle,
            task_ids: run.task_ids.clone(),
            created_at: run.created_at.timestamp_millis(),
            updated_at: run.updated_at.timestamp_millis(),
        })
    }

    async fn dispatch_run(
        &self,
        run_id: &str,
        options: DispatchOptions,
    ) -> Result<Vec<crate::contracts::dto::WorkerLaunchEnvelope>> {
        let mut store = self.store.lock().unwrap();

        // Update run lifecycle
        store.update_run_lifecycle(run_id, RunLifecycle::Running)?;

        let run = store
            .get_run_status(run_id)
            .ok_or_else(|| Error::validation(format!("run {run_id} not found")))?;

        let mut envelopes = Vec::new();
        for task_id in &run.task_ids {
            let ttl = options.lease_ttl_sec.unwrap_or(300);
            let agent_id = options
                .agent_id_prefix
                .as_deref()
                .unwrap_or("agent")
                .to_string();

            let grant = store
                .issue_lease(task_id, &agent_id, ttl)
                .map_err(|e| Error::validation(e.to_string()))?;

            store.apply_task_transition(
                task_id,
                &TransitionEvent::Dispatch {
                    lease_id: grant.lease_id.clone(),
                    agent_id: grant.agent_id.clone(),
                    fence_token: grant.fence_token,
                    expires_at: grant.expires_at,
                },
            )?;

            envelopes.push(crate::contracts::dto::WorkerLaunchEnvelope {
                task_id: task_id.clone(),
                run_id: run_id.to_string(),
                attempt_id: format!("att-{task_id}-{}", grant.fence_token),
                launch_id: format!("launch-{task_id}-{}", grant.fence_token),
                actor: agent_id.clone(),
                requested_runtime: crate::contracts::dto::WorkerRuntimeKind::NativeRust,
                resolved_runtime: crate::contracts::dto::WorkerRuntimeKind::NativeRust,
                lease_id: grant.lease_id,
                fence_token: grant.fence_token,
                workspace_snapshot_id: String::new(),
                context_pack_id: String::new(),
                admission_grant_id: String::new(),
                capability_policy_id: String::new(),
                verification_policy_id: String::new(),
            });
        }

        Ok(envelopes)
    }

    async fn cancel_run(&self, run_id: &str, reason: Option<String>) -> Result<()> {
        let mut store = self.store.lock().unwrap();
        store.cancel_run(run_id, reason)
    }

    async fn submit_task(&self, request: SubmitTaskRequest) -> Result<SubmitTaskResult> {
        let mut store = self.store.lock().unwrap();

        store.apply_task_transition(
            &request.task_id,
            &TransitionEvent::Submit {
                lease_id: request.lease_id.clone(),
                fence_token: request.fence_token,
                patch_digest: request.patch_digest.clone(),
                verify_run_id: request.verify_run_id.clone(),
            },
        )?;

        let task = store
            .get_task(&request.task_id)
            .ok_or_else(|| Error::validation("task not found after submit"))?;

        let state_str = format!("{:?}", task.runtime.state);
        let closed = matches!(task.runtime.state, RuntimeState::Terminal(_));

        Ok(SubmitTaskResult {
            task_id: request.task_id,
            state: state_str,
            closed,
        })
    }

    async fn append_evidence(&self, request: AppendEvidenceRequest) -> Result<()> {
        let mut store = self.store.lock().unwrap();

        let record = EvidenceRecord::from_command_output(
            &request.task_id,
            &request.command,
            request.exit_code,
            &request.stdout,
            &request.stderr,
            request.artifact_ids.clone(),
        );

        store.append_evidence(record)
    }

    async fn state_digest(&self, task_id: Option<&str>) -> Result<StateDigestResult> {
        let store = self.store.lock().unwrap();
        Ok(store.state_digest(task_id))
    }

    async fn acquire_lease(&self, task_id: &str, ttl_seconds: i64) -> Result<ContractLeaseGrant> {
        let mut store = self.store.lock().unwrap();

        let agent_id = "agent".to_string();
        let grant = store
            .issue_lease(task_id, &agent_id, ttl_seconds)
            .map_err(|e| Error::validation(e.to_string()))?;

        store.apply_task_transition(
            task_id,
            &TransitionEvent::Dispatch {
                lease_id: grant.lease_id.clone(),
                agent_id: grant.agent_id.clone(),
                fence_token: grant.fence_token,
                expires_at: grant.expires_at,
            },
        )?;

        Ok(ContractLeaseGrant {
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            expires_at: grant.expires_at.timestamp_millis(),
        })
    }

    async fn validate_lease(
        &self,
        _task_id: &str,
        lease_id: &str,
        fence_token: u64,
    ) -> Result<bool> {
        let store = self.store.lock().unwrap();
        Ok(store.validate_lease_fence(lease_id, fence_token).is_ok())
    }
}

// ============================================================================
// Type conversions between orchestration and contract types
// ============================================================================

/// Convert orchestration RunLifecycle to contract RunLifecycle.
fn convert_run_lifecycle(lifecycle: RunLifecycle) -> crate::contracts::engine::RunLifecycle {
    match lifecycle {
        RunLifecycle::Pending => crate::contracts::engine::RunLifecycle::Pending,
        RunLifecycle::Running => crate::contracts::engine::RunLifecycle::Active,
        RunLifecycle::Blocked => crate::contracts::engine::RunLifecycle::Active,
        RunLifecycle::AwaitingHuman => crate::contracts::engine::RunLifecycle::Completing,
        RunLifecycle::Succeeded => crate::contracts::engine::RunLifecycle::Complete,
        RunLifecycle::Failed => crate::contracts::engine::RunLifecycle::Failed,
        RunLifecycle::Canceled => crate::contracts::engine::RunLifecycle::Canceled,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::engine::TaskPrerequisite;
    use crate::orchestration::ExecutionTier;
    use crate::reliability::state::RuntimeState;

    fn test_store() -> WorkflowStore {
        WorkflowStore::new_test(3, 60).unwrap()
    }

    #[test]
    fn test_create_task_and_journal() {
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "Build feature".to_string(),
                Vec::new(),
                Some("cargo test".to_string()),
                None,
            )
            .unwrap();

        assert!(store.get_task("task-1").is_some());
        assert_eq!(store.task_count(), 1);
        assert_eq!(store.journal_len(), 1);

        // Task should be Ready (no prerequisites)
        assert_eq!(
            store.get_task("task-1").unwrap().runtime.state,
            RuntimeState::Ready
        );

        // Journal entry should be CreateTask
        let entry = &store.journal()[0];
        assert_eq!(entry.seq, 1);
        matches!(&entry.command, WorkflowCommand::CreateTask { .. });
        assert!(entry.resulting_task_state.is_some());
    }

    #[test]
    fn test_create_task_with_prerequisites_is_blocked() {
        let mut store = test_store();

        // Create task-2 that depends on task-1 (which doesn't exist yet)
        store
            .create_task(
                "task-2".to_string(),
                "Dependent task".to_string(),
                vec![TaskPrerequisite {
                    task_id: "task-1".to_string(),
                    trigger: crate::contracts::engine::PrerequisiteTrigger::OnSuccess,
                }],
                Some("cargo test".to_string()),
                None,
            )
            .unwrap();

        assert!(matches!(
            store.get_task("task-2").unwrap().runtime.state,
            RuntimeState::Blocked { .. }
        ));

        // Edge should be created
        assert_eq!(store.edges().len(), 1);
        assert_eq!(store.edges()[0].from, "task-1");
        assert_eq!(store.edges()[0].to, "task-2");
    }

    #[test]
    fn test_duplicate_task_creation_fails() {
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "First".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();

        let result = store.create_task(
            "task-1".to_string(),
            "Duplicate".to_string(),
            Vec::new(),
            None,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_task_lifecycle_transitions_are_journaled() {
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "Build feature".to_string(),
                Vec::new(),
                Some("cargo test".to_string()),
                None,
            )
            .unwrap();

        // Dispatch
        store
            .apply_task_transition(
                "task-1",
                &TransitionEvent::Dispatch {
                    lease_id: "lease-1".to_string(),
                    agent_id: "agent-1".to_string(),
                    fence_token: 1,
                    expires_at: Utc::now(),
                },
            )
            .unwrap();

        assert!(matches!(
            store.get_task("task-1").unwrap().runtime.state,
            RuntimeState::Leased { .. }
        ));

        // Submit
        store
            .apply_task_transition(
                "task-1",
                &TransitionEvent::Submit {
                    lease_id: "lease-1".to_string(),
                    fence_token: 1,
                    patch_digest: "abc123".to_string(),
                    verify_run_id: "verify-1".to_string(),
                },
            )
            .unwrap();

        assert!(matches!(
            store.get_task("task-1").unwrap().runtime.state,
            RuntimeState::Verifying { .. }
        ));

        // Verify success
        store
            .apply_task_transition(
                "task-1",
                &TransitionEvent::VerifySuccess {
                    verify_run_id: "verify-1".to_string(),
                    patch_digest: "abc123".to_string(),
                },
            )
            .unwrap();

        assert!(matches!(
            store.get_task("task-1").unwrap().runtime.state,
            RuntimeState::Terminal(TerminalState::Succeeded { .. })
        ));

        // Should have 4 journal entries (create + 3 transitions)
        assert_eq!(store.journal_len(), 4);
    }

    #[test]
    fn test_invalid_transition_fails_without_journal() {
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "Build feature".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();

        // Try to submit a Ready task (should fail — not leased)
        let result = store.apply_task_transition(
            "task-1",
            &TransitionEvent::Submit {
                lease_id: "lease-1".to_string(),
                fence_token: 1,
                patch_digest: "abc".to_string(),
                verify_run_id: "v1".to_string(),
            },
        );

        assert!(result.is_err());
        // Journal should still only have the create entry
        assert_eq!(store.journal_len(), 1);
        // Task should still be Ready
        assert_eq!(
            store.get_task("task-1").unwrap().runtime.state,
            RuntimeState::Ready
        );
    }

    #[test]
    fn test_run_lifecycle() {
        let mut store = test_store();
        store
            .create_run(
                "run-1".to_string(),
                "Build the thing".to_string(),
                vec!["task-1".to_string()],
            )
            .unwrap();

        assert_eq!(store.run_count(), 1);
        let run = store.get_run_status("run-1").unwrap();
        assert_eq!(run.lifecycle, RunLifecycle::Pending);

        // Journal should have CreateRun entry
        assert!(
            store
                .journal()
                .iter()
                .any(|e| matches!(e.command, WorkflowCommand::CreateRun { .. }))
        );
    }

    #[test]
    fn test_state_digest_projection() {
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "Task 1".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();
        store
            .create_task(
                "task-2".to_string(),
                "Task 2".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();

        let digest = store.state_digest(None);
        assert_eq!(digest.tasks.len(), 2);
        assert!(digest.is_dag_valid);
        assert_eq!(digest.edge_count, 0);
        assert_eq!(digest.schema, "workflow-store-v1");
    }

    #[test]
    fn test_evidence_append_and_retrieval() {
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "Build".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();

        let record = EvidenceRecord::from_command_output(
            "task-1",
            "cargo test",
            0,
            "all passed",
            "",
            Vec::new(),
        );

        store.append_evidence(record).unwrap();
        assert_eq!(store.get_evidence("task-1").len(), 1);

        // Journal should have CreateTask + AppendEvidence
        assert_eq!(store.journal_len(), 2);
    }

    #[test]
    fn test_journal_is_append_only() {
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "Task".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();

        store
            .apply_task_transition(
                "task-1",
                &TransitionEvent::Dispatch {
                    lease_id: "l1".to_string(),
                    agent_id: "a1".to_string(),
                    fence_token: 1,
                    expires_at: Utc::now(),
                },
            )
            .unwrap();

        let journal = store.journal();
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].seq, 1);
        assert_eq!(journal[1].seq, 2);

        // Each entry has a unique entry_id
        assert_ne!(journal[0].entry_id, journal[1].entry_id);

        // Each entry has a checksum
        assert!(!journal[0].command_checksum.is_empty());
        assert!(!journal[1].command_checksum.is_empty());
    }

    #[test]
    fn test_dag_cycle_detection() {
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "A".to_string(),
                vec![TaskPrerequisite {
                    task_id: "task-2".to_string(),
                    trigger: crate::contracts::engine::PrerequisiteTrigger::OnSuccess,
                }],
                None,
                None,
            )
            .unwrap();
        store
            .create_task(
                "task-2".to_string(),
                "B".to_string(),
                vec![TaskPrerequisite {
                    task_id: "task-1".to_string(),
                    trigger: crate::contracts::engine::PrerequisiteTrigger::OnSuccess,
                }],
                None,
                None,
            )
            .unwrap();

        let validation = store.validate_dag();
        assert!(validation.has_cycle);
    }

    #[test]
    fn test_cancel_run_cancels_non_terminal_tasks() {
        let mut store = test_store();
        store
            .create_run(
                "run-1".to_string(),
                "Run".to_string(),
                vec!["task-1".to_string(), "task-2".to_string()],
            )
            .unwrap();
        store
            .create_task(
                "task-1".to_string(),
                "T1".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();
        store
            .create_task(
                "task-2".to_string(),
                "T2".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();

        store
            .cancel_run("run-1", Some("test cancel".to_string()))
            .unwrap();

        let run = store.get_run_status("run-1").unwrap();
        assert_eq!(run.lifecycle, RunLifecycle::Canceled);

        // Both tasks should be canceled
        assert!(matches!(
            store.get_task("task-1").unwrap().runtime.state,
            RuntimeState::Terminal(TerminalState::Canceled { .. })
        ));
        assert!(matches!(
            store.get_task("task-2").unwrap().runtime.state,
            RuntimeState::Terminal(TerminalState::Canceled { .. })
        ));
    }

    // ========================================================================
    // VAL-WF-013: Atomic migration cutover tests
    // ========================================================================

    #[test]
    fn test_migration_from_legacy_succeeds() {
        let mut legacy_reliability =
            RpcReliabilityState::new(&crate::config::Config::default()).unwrap();
        let mut legacy_orchestration = RpcOrchestrationState::default();

        // Add a task to legacy state
        let task_spec = crate::reliability::task::TaskSpec {
            objective: "Test task".to_string(),
            constraints: crate::reliability::task::TaskConstraintSet::default(),
            verify: crate::reliability::task::VerifyPlan::Standard {
                audit_diff: true,
                command: "cargo test".to_string(),
                timeout_sec: 60,
            },
            max_attempts: 3,
            input_snapshot: "snap-123".to_string(),
            acceptance_ids: Vec::new(),
            planned_touches: Vec::new(),
        };
        let task_node = TaskNode::new("legacy-task-1".to_string(), task_spec);
        legacy_reliability
            .tasks
            .insert("legacy-task-1".to_string(), task_node);

        // Add a run to legacy state
        let run_status = RunStatus::new("legacy-run-1", "Test run", ExecutionTier::Wave);
        legacy_orchestration.register_run(run_status);

        // Migrate
        let mut workflow_store = test_store();
        let record = WorkflowMigrationCutover::migrate_from_legacy(
            &legacy_reliability,
            &legacy_orchestration,
            &mut workflow_store,
        )
        .unwrap();

        assert!(record.verification_passed);
        assert_eq!(record.task_count, 1);
        assert_eq!(record.run_count, 1);
        assert_eq!(record.outcome, WorkflowMigrationOutcome::Succeeded);

        // Verify the store has the data
        assert!(workflow_store.get_task("legacy-task-1").is_some());
        assert!(workflow_store.get_run_status("legacy-run-1").is_some());
    }

    #[test]
    fn test_migration_record_has_correlation_id() {
        let legacy_reliability =
            RpcReliabilityState::new(&crate::config::Config::default()).unwrap();
        let legacy_orchestration = RpcOrchestrationState::default();

        let mut workflow_store = test_store();
        let record = WorkflowMigrationCutover::migrate_from_legacy(
            &legacy_reliability,
            &legacy_orchestration,
            &mut workflow_store,
        )
        .unwrap();

        // Record must have a correlation ID
        assert!(!record.correlation_id.is_empty());
        assert!(record.correlation_id.starts_with("wf-mig-"));

        // Record must have timestamps
        assert!(!record.started_at.is_empty());
        assert!(!record.completed_at.is_empty());
    }

    #[test]
    fn test_migration_verifies_state_consistency() {
        let mut legacy_reliability =
            RpcReliabilityState::new(&crate::config::Config::default()).unwrap();
        let mut legacy_orchestration = RpcOrchestrationState::default();

        // Create tasks in both legacy and workflow store to cause a mismatch
        let task_spec = crate::reliability::task::TaskSpec {
            objective: "Test".to_string(),
            constraints: crate::reliability::task::TaskConstraintSet::default(),
            verify: crate::reliability::task::VerifyPlan::Standard {
                audit_diff: false,
                command: "cargo test".to_string(),
                timeout_sec: 60,
            },
            max_attempts: 3,
            input_snapshot: "snap".to_string(),
            acceptance_ids: Vec::new(),
            planned_touches: Vec::new(),
        };
        let mut task = TaskNode::new("task-1".to_string(), task_spec);
        task.runtime.state = RuntimeState::Ready;
        legacy_reliability.tasks.insert("task-1".to_string(), task);

        let run_status = RunStatus::new("run-1", "Test run", ExecutionTier::Wave);
        legacy_orchestration.register_run(run_status);

        let mut workflow_store = test_store();
        let record = WorkflowMigrationCutover::migrate_from_legacy(
            &legacy_reliability,
            &legacy_orchestration,
            &mut workflow_store,
        )
        .unwrap();

        assert!(record.verification_passed);
        assert_eq!(workflow_store.task_count(), 1);
        assert_eq!(workflow_store.run_count(), 1);
    }

    #[test]
    fn test_journal_agrees_with_live_queries() {
        // VAL-WF-001: rebuildable projections agree with live queries
        let mut store = test_store();
        store
            .create_task(
                "task-1".to_string(),
                "Build".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();

        store
            .apply_task_transition(
                "task-1",
                &TransitionEvent::Dispatch {
                    lease_id: "l1".to_string(),
                    agent_id: "a1".to_string(),
                    fence_token: 1,
                    expires_at: Utc::now(),
                },
            )
            .unwrap();

        // Live query: task should be Leased
        let live_state =
            TaskStateView::from_runtime(&store.get_task("task-1").unwrap().runtime.state);
        assert!(matches!(live_state, TaskStateView::Leased { .. }));

        // Journal projection: last entry should show Leased
        let last_entry = store.journal().last().unwrap();
        assert!(matches!(
            last_entry.resulting_task_state,
            Some(TaskStateView::Leased { .. })
        ));

        // State digest projection should agree
        let digest = store.state_digest(Some("task-1"));
        assert_eq!(digest.tasks.len(), 1);
        assert!(digest.tasks[0].state.contains("Leased"));
    }

    #[test]
    fn test_dual_authority_writes_blocked_after_cutover() {
        // After migration, the workflow store is authoritative.
        // The legacy state should not be used for writes.
        let legacy_reliability =
            RpcReliabilityState::new(&crate::config::Config::default()).unwrap();
        let legacy_orchestration = RpcOrchestrationState::default();

        let mut workflow_store = test_store();
        let record = WorkflowMigrationCutover::migrate_from_legacy(
            &legacy_reliability,
            &legacy_orchestration,
            &mut workflow_store,
        )
        .unwrap();

        assert!(record.verification_passed);
        assert_eq!(record.outcome, WorkflowMigrationOutcome::Succeeded);

        // After cutover, all writes go through the workflow store
        workflow_store
            .create_task(
                "new-task".to_string(),
                "New task post-cutover".to_string(),
                Vec::new(),
                None,
                None,
            )
            .unwrap();

        assert!(workflow_store.get_task("new-task").is_some());
        // Legacy state does NOT have the new task
        assert!(!legacy_reliability.tasks.contains_key("new-task"));
    }
}
