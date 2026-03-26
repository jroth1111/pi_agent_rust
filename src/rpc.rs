//! Test-only RPC harness.
//!
//! The live RPC transport/runtime surface now lives under `crate::surface`.
//! This module remains only as the crate-local unit-test harness for the
//! extracted RPC helpers and transport paths.

#![allow(clippy::significant_drop_tightening)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::ignored_unit_patterns)]
#![allow(clippy::needless_pass_by_value)]

#[cfg(test)]
use crate::auth::AuthStorage;
#[cfg(test)]
use crate::config::{Config, ReliabilityEnforcementMode};
#[cfg(test)]
use crate::error::Error;
#[cfg(test)]
use crate::models::ModelEntry;
#[cfg(test)]
use crate::orchestration::RunStatus;
#[cfg(test)]
use crate::reliability;
#[cfg(test)]
use crate::resources::ResourceLoader;
#[cfg(test)]
use crate::surface::rpc_types::RpcOptions;
#[cfg(test)]
pub(crate) use crate::surface::rpc_types::{
    AppendEvidenceRequest, BlockerReport, ClosePayload, DispatchGrant, RpcOrchestrationState,
    RpcReliabilityState, SubmitTaskRequest, SubmitTaskResponse, TaskContract, TaskPrerequisite,
    provider_ids_match,
};
#[cfg(test)]
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use crate::agent::AbortHandle;
#[cfg(test)]
use crate::agent_cx::AgentCx;
#[cfg(test)]
use crate::contracts::dto::SessionIdentity;
#[cfg(test)]
use crate::error::Result;
#[cfg(test)]
use crate::model::StopReason;
#[cfg(test)]
use crate::orchestration::ExecutionTier;
#[cfg(test)]
use crate::orchestration::{
    RunLifecycle, RunStore, RunVerifyStatus, SubrunPlan, TaskReport, WaveStatus,
};
#[cfg(test)]
use crate::services::run_service;
#[cfg(test)]
use crate::services::run_service::CompletedRunVerifyScope;
#[cfg(test)]
use crate::surface::rpc_protocol::{error_hints_value, normalize_command_type};
#[cfg(test)]
use crate::surface::rpc_support::{
    RpcSharedState, RpcStateSnapshot, RpcUiBridgeState, available_thinking_levels,
    current_model_entry, model_requires_configured_credential, resolve_model_key,
    rpc_model_from_entry, sync_agent_queue_modes,
};
#[cfg(test)]
use crate::surface::rpc_transport_commands::{
    StreamingBehavior, build_user_message, is_extension_command, line_count_from_newline_count,
    parse_prompt_images, parse_streaming_behavior, retry_delay_ms, rpc_parse_extension_ui_response,
    rpc_parse_extension_ui_response_id, run_prompt_with_retry, should_auto_compact,
};
#[cfg(test)]
use asupersync::sync::Mutex;
#[cfg(test)]
use serde_json::{Value, json};
#[cfg(test)]
use std::sync::atomic::AtomicBool;

#[cfg(test)]
mod test_support;
#[cfg(test)]
use test_support::*;

#[cfg(test)]
mod tests;
