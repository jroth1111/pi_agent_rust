//! Composition-kernel types for wiring authoritative runtime services.
//!
//! Surfaces should obtain engine/plane access through this bundle rather than
//! constructing ad hoc service sets or reaching into engine internals.

use crate::contracts::engine::{
    ConversationContract, PersistenceContract, WorkerRuntimeContract, WorkflowContract,
};
use crate::contracts::plane::{
    AdmissionContract, CapabilityContract, ContextContract, ExtensionRuntimeContract,
    HostExecutionContract, IdentityContract, InferenceContract, WorkspaceContract,
};
use crate::error::{Error, Result};
use std::sync::Arc;

/// Typed bundle of authoritative runtime services.
#[derive(Clone)]
pub struct ApplicationKernel {
    conversation: Arc<dyn ConversationContract>,
    workflow: Arc<dyn WorkflowContract>,
    persistence: Arc<dyn PersistenceContract>,
    worker_runtime: Arc<dyn WorkerRuntimeContract>,
    inference: Arc<dyn InferenceContract>,
    context: Arc<dyn ContextContract>,
    workspace: Arc<dyn WorkspaceContract>,
    host_execution: Arc<dyn HostExecutionContract>,
    identity: Arc<dyn IdentityContract>,
    capability: Arc<dyn CapabilityContract>,
    admission: Arc<dyn AdmissionContract>,
    extension_runtime: Arc<dyn ExtensionRuntimeContract>,
}

impl ApplicationKernel {
    #[must_use]
    pub fn builder() -> ApplicationKernelBuilder {
        ApplicationKernelBuilder::default()
    }

    #[must_use]
    pub fn conversation(&self) -> Arc<dyn ConversationContract> {
        Arc::clone(&self.conversation)
    }

    #[must_use]
    pub fn workflow(&self) -> Arc<dyn WorkflowContract> {
        Arc::clone(&self.workflow)
    }

    #[must_use]
    pub fn persistence(&self) -> Arc<dyn PersistenceContract> {
        Arc::clone(&self.persistence)
    }

    #[must_use]
    pub fn worker_runtime(&self) -> Arc<dyn WorkerRuntimeContract> {
        Arc::clone(&self.worker_runtime)
    }

    #[must_use]
    pub fn inference(&self) -> Arc<dyn InferenceContract> {
        Arc::clone(&self.inference)
    }

    #[must_use]
    pub fn context(&self) -> Arc<dyn ContextContract> {
        Arc::clone(&self.context)
    }

    #[must_use]
    pub fn workspace(&self) -> Arc<dyn WorkspaceContract> {
        Arc::clone(&self.workspace)
    }

    #[must_use]
    pub fn host_execution(&self) -> Arc<dyn HostExecutionContract> {
        Arc::clone(&self.host_execution)
    }

    #[must_use]
    pub fn identity(&self) -> Arc<dyn IdentityContract> {
        Arc::clone(&self.identity)
    }

    #[must_use]
    pub fn capability(&self) -> Arc<dyn CapabilityContract> {
        Arc::clone(&self.capability)
    }

    #[must_use]
    pub fn admission(&self) -> Arc<dyn AdmissionContract> {
        Arc::clone(&self.admission)
    }

    #[must_use]
    pub fn extension_runtime(&self) -> Arc<dyn ExtensionRuntimeContract> {
        Arc::clone(&self.extension_runtime)
    }
}

/// Builder for [`ApplicationKernel`].
#[derive(Default)]
pub struct ApplicationKernelBuilder {
    conversation: Option<Arc<dyn ConversationContract>>,
    workflow: Option<Arc<dyn WorkflowContract>>,
    persistence: Option<Arc<dyn PersistenceContract>>,
    worker_runtime: Option<Arc<dyn WorkerRuntimeContract>>,
    inference: Option<Arc<dyn InferenceContract>>,
    context: Option<Arc<dyn ContextContract>>,
    workspace: Option<Arc<dyn WorkspaceContract>>,
    host_execution: Option<Arc<dyn HostExecutionContract>>,
    identity: Option<Arc<dyn IdentityContract>>,
    capability: Option<Arc<dyn CapabilityContract>>,
    admission: Option<Arc<dyn AdmissionContract>>,
    extension_runtime: Option<Arc<dyn ExtensionRuntimeContract>>,
}

impl ApplicationKernelBuilder {
    #[must_use]
    pub fn conversation(mut self, service: Arc<dyn ConversationContract>) -> Self {
        self.conversation = Some(service);
        self
    }

    #[must_use]
    pub fn workflow(mut self, service: Arc<dyn WorkflowContract>) -> Self {
        self.workflow = Some(service);
        self
    }

    #[must_use]
    pub fn persistence(mut self, service: Arc<dyn PersistenceContract>) -> Self {
        self.persistence = Some(service);
        self
    }

    #[must_use]
    pub fn worker_runtime(mut self, service: Arc<dyn WorkerRuntimeContract>) -> Self {
        self.worker_runtime = Some(service);
        self
    }

    #[must_use]
    pub fn inference(mut self, service: Arc<dyn InferenceContract>) -> Self {
        self.inference = Some(service);
        self
    }

    #[must_use]
    pub fn context(mut self, service: Arc<dyn ContextContract>) -> Self {
        self.context = Some(service);
        self
    }

    #[must_use]
    pub fn workspace(mut self, service: Arc<dyn WorkspaceContract>) -> Self {
        self.workspace = Some(service);
        self
    }

    #[must_use]
    pub fn host_execution(mut self, service: Arc<dyn HostExecutionContract>) -> Self {
        self.host_execution = Some(service);
        self
    }

    #[must_use]
    pub fn identity(mut self, service: Arc<dyn IdentityContract>) -> Self {
        self.identity = Some(service);
        self
    }

    #[must_use]
    pub fn capability(mut self, service: Arc<dyn CapabilityContract>) -> Self {
        self.capability = Some(service);
        self
    }

    #[must_use]
    pub fn admission(mut self, service: Arc<dyn AdmissionContract>) -> Self {
        self.admission = Some(service);
        self
    }

    #[must_use]
    pub fn extension_runtime(mut self, service: Arc<dyn ExtensionRuntimeContract>) -> Self {
        self.extension_runtime = Some(service);
        self
    }

    /// Build the kernel once all authoritative services are present.
    ///
    /// # Errors
    ///
    /// Returns a validation error if any required service is missing.
    pub fn build(self) -> Result<ApplicationKernel> {
        let Self {
            conversation,
            workflow,
            persistence,
            worker_runtime,
            inference,
            context,
            workspace,
            host_execution,
            identity,
            capability,
            admission,
            extension_runtime,
        } = self;

        Ok(ApplicationKernel {
            conversation: Self::required(conversation, "conversation")?,
            workflow: Self::required(workflow, "workflow")?,
            persistence: Self::required(persistence, "persistence")?,
            worker_runtime: Self::required(worker_runtime, "worker_runtime")?,
            inference: Self::required(inference, "inference")?,
            context: Self::required(context, "context")?,
            workspace: Self::required(workspace, "workspace")?,
            host_execution: Self::required(host_execution, "host_execution")?,
            identity: Self::required(identity, "identity")?,
            capability: Self::required(capability, "capability")?,
            admission: Self::required(admission, "admission")?,
            extension_runtime: Self::required(extension_runtime, "extension_runtime")?,
        })
    }

    fn required<T>(value: Option<Arc<T>>, name: &str) -> Result<Arc<T>>
    where
        T: ?Sized,
    {
        value
            .ok_or_else(|| Error::validation(format!("missing ApplicationKernel service `{name}`")))
    }
}
