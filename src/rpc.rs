//! RPC mode: headless JSON protocol over stdin/stdout.
//!
//! This implements a compatibility subset of pi-mono's RPC protocol
//! (see legacy `docs/rpc.md` in `legacy_pi_mono_code`).

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
    AppendEvidenceRequest, ArtifactQuery, BlockerReport, CancelRunRequest, ClosePayload,
    DispatchGrant, DispatchRunRequest, EvidenceRecord, RpcOrchestrationState, RpcReliabilityState,
    RpcScopedModel, RunLookupRequest, StartRunRequest, StateDigest, StateDigestRequest,
    SubmitTaskRequest, SubmitTaskResponse, TaskContract, TaskPrerequisite, provider_ids_match,
};
#[cfg(test)]
use asupersync::runtime::RuntimeHandle;
#[cfg(test)]
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use crate::agent::{AbortHandle, AgentEvent};
#[cfg(test)]
use crate::agent_cx::AgentCx;
#[cfg(test)]
use crate::compaction::{
    ResolvedCompactionSettings, compact, compaction_details_to_value, prepare_compaction,
};
#[cfg(test)]
use crate::contracts::dto::SessionIdentity;
#[cfg(test)]
use crate::extensions::ExtensionManager;
#[cfg(test)]
use crate::model::{ContentBlock, ImageContent, StopReason, TextContent, UserContent, UserMessage};
#[cfg(test)]
use crate::orchestration::ExecutionTier;
#[cfg(test)]
use crate::orchestration::{
    RunLifecycle, RunStore, RunVerifyStatus, SubrunPlan, TaskReport, WaveStatus,
};
#[cfg(test)]
use crate::services::reliability_service::ReliabilityService;
#[cfg(test)]
use crate::services::run_service;
#[cfg(test)]
use crate::services::run_service::CompletedRunVerifyScope;
#[cfg(test)]
use crate::surface::rpc_protocol::{error_hints_value, normalize_command_type, response_error};
#[cfg(test)]
use crate::surface::rpc_server::try_send_line_with_backpressure;
#[cfg(test)]
use crate::surface::rpc_support::{
    RpcSharedState, RpcStateSnapshot, RpcUiBridgeState, RunningBash, apply_model_change,
    apply_thinking_level, available_thinking_levels, current_model_entry, cycle_model_for_rpc,
    export_html_snapshot, fork_messages_from_entries, last_assistant_text,
    model_requires_configured_credential, resolve_model_key, rpc_model_from_entry,
    rpc_session_message_value, session_state, session_stats, sync_agent_queue_modes,
};
#[cfg(test)]
use crate::surface::rpc_transport_commands::{
    StreamingBehavior, build_user_message, is_extension_command, line_count_from_newline_count,
    parse_prompt_images, parse_streaming_behavior, retry_delay_ms, rpc_parse_extension_ui_response,
    rpc_parse_extension_ui_response_id, run_bash_rpc, run_prompt_with_retry, should_auto_compact,
};
#[cfg(test)]
use asupersync::channel::oneshot;
#[cfg(test)]
use asupersync::sync::Mutex;
#[cfg(test)]
use serde_json::{Value, json};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
mod test_support;
#[cfg(test)]
use test_support::*;

#[cfg(test)]
mod tests;
