//! Runtime bootstrap outcome contracts.
//!
//! These types let surfaces observe the result of kernel startup without
//! reaching into concrete engine implementations.

use crate::contracts::bootstrap::{InteractionMode, SurfaceKind};
use serde::{Deserialize, Serialize};

/// Service health classification exposed during bootstrap.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Ready,
    Degraded,
    Unavailable,
}

impl ServiceState {
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Aggregate kernel readiness snapshot returned after bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelReadiness {
    pub conversation: ServiceState,
    pub workflow: ServiceState,
    pub persistence: ServiceState,
    pub worker_runtime: ServiceState,
    pub inference: ServiceState,
    pub context: ServiceState,
    pub workspace: ServiceState,
    pub host_execution: ServiceState,
    pub identity: ServiceState,
    pub capability: ServiceState,
    pub admission: ServiceState,
    pub extension_runtime: ServiceState,
}

impl KernelReadiness {
    #[must_use]
    pub const fn fully_ready() -> Self {
        Self {
            conversation: ServiceState::Ready,
            workflow: ServiceState::Ready,
            persistence: ServiceState::Ready,
            worker_runtime: ServiceState::Ready,
            inference: ServiceState::Ready,
            context: ServiceState::Ready,
            workspace: ServiceState::Ready,
            host_execution: ServiceState::Ready,
            identity: ServiceState::Ready,
            capability: ServiceState::Ready,
            admission: ServiceState::Ready,
            extension_runtime: ServiceState::Ready,
        }
    }

    #[must_use]
    pub fn all_required_ready(&self, requires_extension_runtime: bool) -> bool {
        self.conversation.is_ready()
            && self.workflow.is_ready()
            && self.persistence.is_ready()
            && self.worker_runtime.is_ready()
            && self.inference.is_ready()
            && self.context.is_ready()
            && self.workspace.is_ready()
            && self.host_execution.is_ready()
            && self.identity.is_ready()
            && self.capability.is_ready()
            && self.admission.is_ready()
            && (!requires_extension_runtime || self.extension_runtime.is_ready())
    }
}

/// Non-fatal condition surfaced during startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapWarning {
    pub code: String,
    pub message: String,
}

impl BootstrapWarning {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Typed result returned after a surface boots the runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapOutcome {
    pub surface: SurfaceKind,
    pub interaction_mode: InteractionMode,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub resumed: bool,
    pub readiness: KernelReadiness,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<BootstrapWarning>,
}

impl BootstrapOutcome {
    #[must_use]
    pub fn new(
        surface: SurfaceKind,
        interaction_mode: InteractionMode,
        session_id: impl Into<String>,
        readiness: KernelReadiness,
    ) -> Self {
        Self {
            surface,
            interaction_mode,
            session_id: session_id.into(),
            workflow_run_id: None,
            workspace_id: None,
            resumed: false,
            readiness,
            warnings: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_workflow_run_id(mut self, workflow_run_id: impl Into<String>) -> Self {
        self.workflow_run_id = Some(workflow_run_id.into());
        self
    }

    #[must_use]
    pub fn with_workspace_id(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    #[must_use]
    pub const fn with_resumed(mut self, resumed: bool) -> Self {
        self.resumed = resumed;
        self
    }

    #[must_use]
    pub fn with_warning(mut self, code: impl Into<String>, message: impl Into<String>) -> Self {
        self.warnings.push(BootstrapWarning::new(code, message));
        self
    }

    #[must_use]
    pub fn is_ready_for_execution(&self, requires_extension_runtime: bool) -> bool {
        self.readiness
            .all_required_ready(requires_extension_runtime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fully_ready_kernel_is_execution_ready() {
        let outcome = BootstrapOutcome::new(
            SurfaceKind::Interactive,
            InteractionMode::Interactive,
            "session-1",
            KernelReadiness::fully_ready(),
        )
        .with_workflow_run_id("run-1")
        .with_workspace_id("workspace-1")
        .with_resumed(true);

        assert!(outcome.is_ready_for_execution(true));
        assert!(outcome.resumed);
        assert_eq!(outcome.workflow_run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn missing_extension_runtime_can_be_accepted_for_non_extension_surfaces() {
        let mut readiness = KernelReadiness::fully_ready();
        readiness.extension_runtime = ServiceState::Unavailable;

        let outcome = BootstrapOutcome::new(
            SurfaceKind::Rpc,
            InteractionMode::Headless,
            "session-2",
            readiness,
        );

        assert!(outcome.is_ready_for_execution(false));
        assert!(!outcome.is_ready_for_execution(true));
    }

    #[test]
    fn warnings_accumulate_without_changing_identity_fields() {
        let outcome = BootstrapOutcome::new(
            SurfaceKind::Cli,
            InteractionMode::Batch,
            "session-3",
            KernelReadiness::fully_ready(),
        )
        .with_warning("degraded_context", "context index rebuild scheduled")
        .with_warning("pending_gc", "artifact garbage collection deferred");

        assert_eq!(outcome.session_id, "session-3");
        assert_eq!(outcome.warnings.len(), 2);
        assert_eq!(outcome.warnings[0].code, "degraded_context");
    }
}
