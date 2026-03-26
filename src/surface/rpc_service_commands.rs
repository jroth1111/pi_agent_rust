use crate::agent::AgentSession;
use crate::agent_cx::AgentCx;
use crate::config::Config;
use crate::orchestration::RunStore;
use crate::services::reliability_service::ReliabilityService;
use crate::services::run_service;
use crate::surface::rpc_protocol::{
    command_payload, parse_command_payload, response_error, response_error_with_hints, response_ok,
};
use crate::surface::rpc_types::{
    AppendEvidenceRequest, ArtifactQuery, BlockerReport, CancelRunRequest, DispatchRunRequest,
    RpcOrchestrationState, RpcReliabilityState, RunLookupRequest, StartRunRequest,
    StateDigestRequest, SubmitTaskRequest, TaskContract,
};
use asupersync::sync::Mutex;
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct RpcServiceCommandContext<'a> {
    pub(crate) cx: &'a AgentCx,
    pub(crate) session: &'a Arc<Mutex<AgentSession>>,
    pub(crate) reliability_state: &'a Arc<Mutex<RpcReliabilityState>>,
    pub(crate) orchestration_state: &'a Arc<Mutex<RpcOrchestrationState>>,
    pub(crate) run_store: &'a RunStore,
    pub(crate) config: &'a Config,
}

pub(crate) async fn handle_service_command(
    parsed: &Value,
    command_type: &str,
    id: Option<String>,
    ctx: RpcServiceCommandContext<'_>,
) -> Option<String> {
    match command_type {
        "orchestration.start_run" => {
            let req: StartRunRequest = match parse_command_payload(parsed, command_type) {
                Ok(req) => req,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match run_service::rpc_start_run(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                ctx.orchestration_state,
                ctx.run_store,
                ctx.config,
                req,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "orchestration.get_run" => {
            let req: RunLookupRequest = match parse_command_payload(parsed, command_type) {
                Ok(req) => req,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match run_service::rpc_get_run(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                ctx.orchestration_state,
                ctx.run_store,
                req,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "orchestration.dispatch_run" => {
            let req: DispatchRunRequest = match parse_command_payload(parsed, command_type) {
                Ok(req) => req,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match run_service::rpc_dispatch_run(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                ctx.orchestration_state,
                ctx.run_store,
                ctx.config,
                req,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "orchestration.cancel_run" => {
            let req: CancelRunRequest = match parse_command_payload(parsed, command_type) {
                Ok(req) => req,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match run_service::rpc_cancel_run(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                ctx.orchestration_state,
                ctx.run_store,
                req,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "orchestration.resume_run" => {
            let req: RunLookupRequest = match parse_command_payload(parsed, command_type) {
                Ok(req) => req,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match run_service::rpc_resume_run(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                ctx.orchestration_state,
                ctx.run_store,
                ctx.config,
                req,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "reliability.request_dispatch" => {
            let payload = command_payload(parsed);
            let contract = payload
                .get("contract")
                .cloned()
                .unwrap_or_else(|| payload.clone());
            let contract: TaskContract = match serde_json::from_value(contract) {
                Ok(contract) => contract,
                Err(err) => {
                    return Some(response_error(
                        id,
                        command_type,
                        format!("Invalid contract payload: {err}"),
                    ));
                }
            };
            let agent_id = payload
                .get("agentId")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("rpc");
            let lease_ttl_sec = payload
                .get("leaseTtlSec")
                .and_then(Value::as_i64)
                .unwrap_or(3600);

            let data = match ReliabilityService::rpc_request_dispatch(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                ctx.orchestration_state,
                ctx.run_store,
                &contract,
                agent_id,
                lease_ttl_sec,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "reliability.append_evidence" => {
            let req: AppendEvidenceRequest = match parse_command_payload(parsed, command_type) {
                Ok(req) => req,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match ReliabilityService::rpc_append_evidence(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                req,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "reliability.submit_task" => {
            let req: SubmitTaskRequest = match parse_command_payload(parsed, command_type) {
                Ok(req) => req,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match ReliabilityService::rpc_submit_task(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                ctx.orchestration_state,
                ctx.run_store,
                req,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "reliability.resolve_blocker" => {
            let report: BlockerReport = match parse_command_payload(parsed, command_type) {
                Ok(report) => report,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match ReliabilityService::rpc_resolve_blocker(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                ctx.orchestration_state,
                ctx.run_store,
                report,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        "reliability.query_artifact" => {
            let payload = command_payload(parsed);
            if let Some(artifact_id) = payload.get("artifactId").and_then(Value::as_str) {
                return Some(
                    match ReliabilityService::rpc_query_artifact_text(
                        ctx.cx,
                        ctx.reliability_state,
                        artifact_id,
                    )
                    .await
                    {
                        Ok(data) => response_ok(id, command_type, Some(data)),
                        Err(err) => response_error_with_hints(id, command_type, &err),
                    },
                );
            }

            let query: ArtifactQuery = match parse_command_payload(parsed, command_type) {
                Ok(query) => query,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            Some(
                match ReliabilityService::rpc_query_artifact_ids(
                    ctx.cx,
                    ctx.reliability_state,
                    query,
                )
                .await
                {
                    Ok(data) => response_ok(id, command_type, Some(data)),
                    Err(err) => response_error_with_hints(id, command_type, &err),
                },
            )
        }
        "reliability.get_state_digest" => {
            let req: StateDigestRequest = match parse_command_payload(parsed, command_type) {
                Ok(req) => req,
                Err(err) => {
                    return Some(response_error_with_hints(id, command_type, &err));
                }
            };
            let data = match ReliabilityService::rpc_get_state_digest(
                ctx.cx,
                ctx.session,
                ctx.reliability_state,
                req.task_id,
            )
            .await
            {
                Ok(data) => data,
                Err(err) => return Some(response_error_with_hints(id, command_type, &err)),
            };
            Some(response_ok(id, command_type, Some(data)))
        }
        _ => None,
    }
}
