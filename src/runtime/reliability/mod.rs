//! Reliability subsystem for typed execution control, evidence capture, and
//! dependency-aware task orchestration.

pub mod artifacts;
pub mod close_guard;
pub mod context_budget;
pub mod dag;
pub mod edge;
pub mod evidence;
pub mod lease;
pub mod recovery;
pub mod retry;
pub mod state;
pub mod state_machine;
pub mod stuck;
pub mod task;
pub mod test_quality;
pub mod verifier;

pub use artifacts::{ArtifactQuery, ArtifactStore, FsArtifactStore};
pub use close_guard::{CloseGuardError, CloseOutcomeKind, ClosePayload, CloseResult};
pub use context_budget::{
    BudgetedContext, ContextBudgetComposer, ContextFanout, ContextSection, RetrievalAdapter,
    RetrievalContextInput, RetrievalPriority,
};
pub use dag::{DagEvaluation, DagEvaluator, DagValidation};
pub use edge::{EdgeKind, EdgeTrigger, FailureClassMask, ReliabilityEdge};
pub use evidence::{EvidenceRecord, EvidenceStatus, StateDigest};
pub use lease::{InMemoryExternalLeaseProvider, LeaseError, LeaseManager, LeaseProvider};
pub use recovery::{RecoveryAction, RecoveryManager};
pub use retry::{
    Attempt, AttemptComparison, AttemptStats, RetryAgent, RetryConfig, RetryStats,
    ReviewSubmission, ReviewerScorer, TokenUsage, VerificationOutcome,
};
pub use state::{DeferTrigger, FailureClass, RuntimeState, TerminalState};
pub use state_machine::{TransitionError, TransitionEvent, apply_transition};
pub use stuck::{StuckDetector, StuckPattern, StuckSeverity};
pub use task::{
    NetworkPolicy, SpecValidationError, TaskConstraintSet, TaskNode, TaskRuntime, TaskSpec,
    VerifyPlan,
};
pub use test_quality::{TautologyFinding, detect_tautological_test_patterns};
pub use verifier::{ScopeAuditRule, VerificationError, VerificationResult, Verifier};
