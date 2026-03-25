use crate::config::ReliabilityEnforcementMode;
use crate::error::{Error, Result};
use crate::reliability;
use crate::reliability::ArtifactStore;
use crate::rpc::{
    AppendEvidenceRequest, ClosePayload, DispatchGrant, EvidenceRecord, RpcReliabilityState,
    StateDigest, SubmitTaskRequest, SubmitTaskResponse, TaskContract,
};
use chrono::SecondsFormat;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub(crate) struct ReliabilityService {
    state: Arc<Mutex<RpcReliabilityState>>,
}

impl ReliabilityService {
    #[must_use]
    pub(crate) const fn new(state: Arc<Mutex<RpcReliabilityState>>) -> Self {
        Self { state }
    }

    pub(crate) async fn append_evidence(
        &self,
        req: AppendEvidenceRequest,
    ) -> Result<EvidenceRecord> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| Error::session(format!("reliability lock failed: {e}")))?;
        Self::append_evidence_state(&mut state, req)
    }

    pub(crate) async fn submit_task(&self, req: SubmitTaskRequest) -> Result<SubmitTaskResponse> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| Error::session(format!("reliability lock failed: {e}")))?;
        Self::submit_task_state(&mut state, req)
    }

    pub(crate) async fn get_state_digest(&self, task_id: &str) -> Result<StateDigest> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| Error::session(format!("reliability lock failed: {e}")))?;
        Self::get_state_digest_state(&mut state, task_id)
    }

    pub(crate) async fn request_dispatch_existing(
        &self,
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> Result<DispatchGrant> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| Error::session(format!("reliability lock failed: {e}")))?;
        Self::request_dispatch_existing_state(&mut state, task_id, agent_id, ttl_seconds)
    }

    pub(crate) async fn first_task_id(&self) -> Result<Option<String>> {
        let state = self
            .state
            .lock()
            .map_err(|e| Error::session(format!("reliability lock failed: {e}")))?;
        Ok(state.first_task_id())
    }

    pub(crate) async fn validate_fence(&self, lease_id: &str, fence_token: u64) -> Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|e| Error::session(format!("reliability lock failed: {e}")))?;
        match state.leases.validate_fence(lease_id, fence_token) {
            Ok(()) => Ok(true),
            Err(reliability::lease::LeaseError::FenceMismatch { .. }) => Ok(false),
            Err(reliability::lease::LeaseError::LeaseNotFound(_)) => Ok(false),
            Err(err) => Err(Error::validation(err.to_string())),
        }
    }

    pub(crate) const fn mode_blocks(state: &RpcReliabilityState) -> bool {
        matches!(state.enforcement_mode, ReliabilityEnforcementMode::Hard)
    }

    pub(crate) fn is_synthetic_trace_parent(trace_parent: &str) -> bool {
        let normalized = trace_parent.trim().to_ascii_lowercase();
        matches!(
            normalized.as_str(),
            "" | "rpc:auto"
                | "auto"
                | "default"
                | "unknown"
                | "none"
                | "n/a"
                | "na"
                | "todo"
                | "tbd"
        )
    }

    pub(crate) fn validate_close_trace_chain(
        state: &RpcReliabilityState,
        task_id: &str,
        close_payload: &ClosePayload,
    ) -> Result<()> {
        let trace_parent = close_payload
            .trace_parent
            .as_ref()
            .map(|trace| trace.trim())
            .filter(|trace| !trace.is_empty())
            .ok_or_else(|| {
                Error::validation("close payload missing parent-goal trace reference")
            })?;

        if Self::is_synthetic_trace_parent(trace_parent) {
            return Err(Error::validation(format!(
                "close trace_parent `{trace_parent}` is synthetic; provide real parent-goal trace ID"
            )));
        }

        let expected = state
            .parent_goal_trace_by_task
            .get(task_id)
            .map(|trace| trace.trim())
            .filter(|trace| !trace.is_empty())
            .ok_or_else(|| {
                Error::validation(format!(
                    "task `{task_id}` missing parent_goal_trace_id in dispatch contract"
                ))
            })?;

        if expected != trace_parent {
            return Err(Error::validation(format!(
                "trace chain mismatch: task `{task_id}` expects parent-goal trace `{expected}`, got `{trace_parent}`"
            )));
        }

        Ok(())
    }

    pub(crate) const fn state_label(state: &reliability::RuntimeState) -> &'static str {
        match state {
            reliability::RuntimeState::Blocked { .. } => "blocked",
            reliability::RuntimeState::Ready => "ready",
            reliability::RuntimeState::Leased { .. } => "leased",
            reliability::RuntimeState::Verifying { .. } => "verifying",
            reliability::RuntimeState::Recoverable { .. } => "recoverable",
            reliability::RuntimeState::AwaitingHuman { .. } => "awaiting_human",
            reliability::RuntimeState::Terminal(_) => "terminal",
        }
    }

    pub(crate) fn task_counts_for(
        state: &RpcReliabilityState,
        task_ids: &[String],
    ) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for task_id in task_ids {
            if let Some(task) = state.tasks.get(task_id) {
                let label = Self::state_label(&task.runtime.state).to_string();
                *counts.entry(label).or_insert(0) += 1;
            }
        }
        counts
    }

    pub(crate) fn get_or_create_task<'a>(
        state: &'a mut RpcReliabilityState,
        contract: &TaskContract,
    ) -> Result<&'a mut reliability::TaskNode> {
        let verify_command = if contract.verify_command.trim().is_empty() {
            "cargo test".to_string()
        } else {
            contract.verify_command.clone()
        };
        let spec = reliability::TaskSpec {
            objective: contract.objective.clone(),
            constraints: reliability::TaskConstraintSet {
                invariants: contract.invariants.clone(),
                max_touched_files: contract.max_touched_files.or(Some(state.max_touched_files)),
                forbid_paths: contract.forbid_paths.clone(),
                network_access: reliability::NetworkPolicy::Offline,
            },
            verify: reliability::VerifyPlan::Standard {
                audit_diff: true,
                command: verify_command,
                timeout_sec: contract
                    .verify_timeout_sec
                    .unwrap_or(state.verify_timeout_sec_default),
            },
            max_attempts: contract.max_attempts.unwrap_or(state.default_max_attempts),
            input_snapshot: contract
                .input_snapshot
                .clone()
                .unwrap_or_else(|| "rpc-dispatch".to_string()),
            acceptance_ids: contract.acceptance_ids.clone(),
            planned_touches: contract.planned_touches.clone(),
        };
        spec.validate()
            .map_err(|err| Error::validation(err.to_string()))?;

        let entry = state
            .tasks
            .entry(contract.task_id.clone())
            .or_insert_with(|| reliability::TaskNode::new(contract.task_id.clone(), spec.clone()));
        entry.spec = spec;
        state.symbol_drift_required_by_task.insert(
            contract.task_id.clone(),
            contract.enforce_symbol_drift_check,
        );
        if let Some(parent_trace) = contract
            .parent_goal_trace_id
            .as_ref()
            .map(|trace| trace.trim())
            .filter(|trace| !trace.is_empty())
        {
            state
                .parent_goal_trace_by_task
                .insert(contract.task_id.clone(), parent_trace.to_string());
        } else {
            state.parent_goal_trace_by_task.remove(&contract.task_id);
        }
        Ok(entry)
    }

    pub(crate) fn trigger_satisfied(
        trigger: &reliability::EdgeTrigger,
        terminal: &reliability::TerminalState,
    ) -> bool {
        match trigger {
            reliability::EdgeTrigger::Always => true,
            reliability::EdgeTrigger::OnSuccess => {
                matches!(terminal, reliability::TerminalState::Succeeded { .. })
            }
            reliability::EdgeTrigger::OnFailure(mask) => match terminal {
                reliability::TerminalState::Failed { class, .. } => mask.matches(*class),
                _ => false,
            },
        }
    }

    pub(crate) fn trigger_key(trigger: &reliability::EdgeTrigger) -> String {
        match trigger {
            reliability::EdgeTrigger::OnSuccess => "on_success".to_string(),
            reliability::EdgeTrigger::Always => "always".to_string(),
            reliability::EdgeTrigger::OnFailure(mask) => {
                let mut classes: Vec<String> = mask
                    .classes
                    .iter()
                    .map(|class| format!("{class:?}"))
                    .collect();
                classes.sort();
                format!("on_failure:{}", classes.join("|"))
            }
        }
    }

    pub(crate) fn reconcile_prerequisites(
        state: &mut RpcReliabilityState,
        contract: &TaskContract,
    ) -> Result<()> {
        let previous_edges = state.edges.clone();
        state.edges.retain(|edge| {
            !(edge.to == contract.task_id
                && matches!(edge.kind, reliability::EdgeKind::Prerequisite { .. }))
        });

        let mut dedupe = HashSet::new();
        for prerequisite in &contract.prerequisites {
            let from = prerequisite.task_id.trim();
            if from.is_empty() {
                continue;
            }
            let key = format!(
                "{}->{}:{}",
                from,
                contract.task_id,
                Self::trigger_key(&prerequisite.trigger)
            );
            if !dedupe.insert(key.clone()) {
                continue;
            }
            state.edges.push(reliability::ReliabilityEdge {
                id: format!("rpc_edge:{key}"),
                from: from.to_string(),
                to: contract.task_id.clone(),
                kind: reliability::EdgeKind::Prerequisite {
                    trigger: prerequisite.trigger.clone(),
                },
            });
        }
        state.edges.sort_by(|left, right| left.id.cmp(&right.id));

        if reliability::DagEvaluator::detect_cycle(&state.edges) {
            state.edges = previous_edges;
            return Err(Error::validation(format!(
                "reliability DAG cycle detected while integrating prerequisites for {}",
                contract.task_id
            )));
        }

        Ok(())
    }

    pub(crate) fn waiting_on_for_task(state: &RpcReliabilityState, task_id: &str) -> Vec<String> {
        let terminal_states: HashMap<String, reliability::TerminalState> = state
            .tasks
            .iter()
            .filter_map(|(id, task)| match &task.runtime.state {
                reliability::RuntimeState::Terminal(terminal) => {
                    Some((id.clone(), terminal.clone()))
                }
                _ => None,
            })
            .collect();
        let mut waiting_on = Vec::new();
        for edge in &state.edges {
            if edge.to != task_id {
                continue;
            }
            let reliability::EdgeKind::Prerequisite { trigger } = &edge.kind else {
                continue;
            };
            match terminal_states.get(&edge.from) {
                Some(terminal) if Self::trigger_satisfied(trigger, terminal) => {}
                _ => waiting_on.push(edge.from.clone()),
            }
        }
        waiting_on.sort();
        waiting_on.dedup();
        waiting_on
    }

    pub(crate) fn project_waiting_on_for_task(
        state: &mut RpcReliabilityState,
        task_id: &str,
    ) -> Vec<String> {
        let waiting_on = Self::waiting_on_for_task(state, task_id);
        let Some(task) = state.tasks.get_mut(task_id) else {
            return waiting_on;
        };

        match (&task.runtime.state, waiting_on.is_empty()) {
            (
                reliability::RuntimeState::Ready | reliability::RuntimeState::Blocked { .. },
                false,
            ) => {
                task.runtime.state = reliability::RuntimeState::Blocked {
                    waiting_on: waiting_on.clone(),
                };
            }
            (reliability::RuntimeState::Blocked { .. }, true) => {
                if reliability::apply_transition(
                    &mut task.runtime,
                    &reliability::TransitionEvent::DependenciesMet,
                    task.spec.max_attempts,
                )
                .is_err()
                {
                    task.runtime.state = reliability::RuntimeState::Ready;
                }
            }
            _ => {}
        }

        waiting_on
    }

    pub(crate) fn evaluate_dag_unblock(
        state: &mut RpcReliabilityState,
    ) -> reliability::DagEvaluation {
        let mut ordered_ids = state.tasks.keys().cloned().collect::<Vec<_>>();
        ordered_ids.sort();
        let mut task_snapshot = ordered_ids
            .iter()
            .filter_map(|task_id| state.tasks.get(task_id).cloned())
            .collect::<Vec<_>>();

        let evaluation =
            reliability::DagEvaluator::evaluate_and_unblock(&mut task_snapshot, &state.edges);
        for node in task_snapshot {
            if let Some(task) = state.tasks.get_mut(&node.id) {
                task.runtime = node.runtime;
            }
        }
        evaluation
    }

    pub(crate) fn promote_recoverable_due(
        state: &mut RpcReliabilityState,
    ) -> Vec<reliability::RecoveryAction> {
        let mut ordered_ids = state.tasks.keys().cloned().collect::<Vec<_>>();
        ordered_ids.sort();
        let mut task_snapshot = ordered_ids
            .iter()
            .filter_map(|task_id| state.tasks.get(task_id).cloned())
            .collect::<Vec<_>>();

        let promoted = reliability::RecoveryManager::promote_recoverable(&mut task_snapshot);
        for node in task_snapshot {
            if let Some(task) = state.tasks.get_mut(&node.id) {
                task.runtime = node.runtime;
            }
        }
        promoted
    }

    pub(crate) fn refresh_dependency_states(state: &mut RpcReliabilityState) {
        Self::promote_recoverable_due(state);
        Self::evaluate_dag_unblock(state);

        let mut ordered_ids = state.tasks.keys().cloned().collect::<Vec<_>>();
        ordered_ids.sort();
        for task_id in ordered_ids {
            Self::project_waiting_on_for_task(state, &task_id);
        }
    }

    pub(crate) fn request_dispatch(
        state: &mut RpcReliabilityState,
        contract: &TaskContract,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> Result<DispatchGrant> {
        state.ensure_enabled()?;
        let task_id = contract.task_id.clone();
        Self::get_or_create_task(state, contract)?;
        state.reconcile_prerequisites(contract)?;
        state.refresh_dependency_states();
        Self::request_dispatch_existing_state(state, &task_id, agent_id, ttl_seconds)
    }

    pub(crate) fn request_dispatch_existing_state(
        state: &mut RpcReliabilityState,
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> Result<DispatchGrant> {
        state.ensure_enabled()?;
        let waiting_on = state.project_waiting_on_for_task(task_id);
        if !waiting_on.is_empty() {
            return Err(Error::validation(format!(
                "task {task_id} is blocked by prerequisites: {}",
                waiting_on.join(", ")
            )));
        }
        if let Some(task) = state.tasks.get(task_id) {
            if let reliability::RuntimeState::Recoverable { retry_after, .. } = &task.runtime.state
            {
                let retry_note = retry_after
                    .map(|ts| ts.to_rfc3339_opts(SecondsFormat::Secs, true))
                    .map(|ts| format!("retry after {ts}"))
                    .unwrap_or_else(|| "retry window not yet promoted".to_string());
                return Err(Error::validation(format!(
                    "task {task_id} is recoverable and not ready to dispatch ({retry_note})"
                )));
            }
        }

        let grant = state
            .leases
            .issue_lease(task_id, agent_id, ttl_seconds)
            .map_err(|err| Error::validation(err.to_string()))?;

        let event = reliability::TransitionEvent::Dispatch {
            lease_id: grant.lease_id.clone(),
            agent_id: grant.agent_id.clone(),
            fence_token: grant.fence_token,
            expires_at: grant.expires_at,
        };

        let mode_blocks = Self::mode_blocks(state);
        let (task_id, state_label) = {
            let Some(task) = state.tasks.get_mut(task_id) else {
                return Err(Error::validation("task disappeared during dispatch"));
            };

            if let Err(err) =
                reliability::apply_transition(&mut task.runtime, &event, task.spec.max_attempts)
            {
                let _ = state.leases.expire_lease(&grant.lease_id);
                if mode_blocks {
                    return Err(Error::validation(format!(
                        "dispatch transition rejected: {err}"
                    )));
                }
                task.runtime.state = reliability::RuntimeState::Ready;
                reliability::apply_transition(&mut task.runtime, &event, task.spec.max_attempts)
                    .map_err(|inner| {
                        Error::validation(format!("dispatch transition recovery failed: {inner}"))
                    })?;
            }
            (
                task.id.clone(),
                Self::state_label(&task.runtime.state).to_string(),
            )
        };

        Ok(DispatchGrant {
            task_id,
            agent_id: grant.agent_id,
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            expires_at: grant.expires_at,
            state: state_label,
        })
    }

    pub(crate) fn expire_dispatch_grant(
        state: &mut RpcReliabilityState,
        grant: &DispatchGrant,
    ) -> Result<()> {
        state.ensure_enabled()?;
        let _ = state.leases.expire_lease(&grant.lease_id);
        let Some(task) = state.tasks.get_mut(&grant.task_id) else {
            return Ok(());
        };

        if matches!(task.runtime.state, reliability::RuntimeState::Leased { .. }) {
            reliability::apply_transition(
                &mut task.runtime,
                &reliability::TransitionEvent::ExpireLease,
                task.spec.max_attempts,
            )
            .map_err(|err| Error::validation(format!("dispatch rollback failed: {err}")))?;
        }
        Ok(())
    }

    pub(crate) fn append_evidence_state(
        state: &mut RpcReliabilityState,
        req: AppendEvidenceRequest,
    ) -> Result<EvidenceRecord> {
        state.ensure_enabled()?;

        let mut artifact_ids = req.artifact_ids;
        if !req.stdout.is_empty() {
            let id = state
                .artifacts
                .put_text(&req.task_id, "stdout", &req.stdout)
                .map_err(|err| Error::session(format!("store stdout artifact failed: {err}")))?;
            artifact_ids.push(id);
        }
        if !req.stderr.is_empty() {
            let id = state
                .artifacts
                .put_text(&req.task_id, "stderr", &req.stderr)
                .map_err(|err| Error::session(format!("store stderr artifact failed: {err}")))?;
            artifact_ids.push(id);
        }

        let evidence = EvidenceRecord::from_command_output_with_env(
            req.task_id.clone(),
            req.command,
            req.exit_code,
            &req.stdout,
            &req.stderr,
            artifact_ids,
            req.env_id,
        );
        state
            .evidence_by_task
            .entry(req.task_id)
            .or_default()
            .push(evidence.clone());
        Ok(evidence)
    }

    pub(crate) fn submit_task_state(
        state: &mut RpcReliabilityState,
        req: SubmitTaskRequest,
    ) -> Result<SubmitTaskResponse> {
        state.ensure_enabled()?;
        let SubmitTaskRequest {
            task_id,
            lease_id,
            fence_token,
            patch_digest,
            verify_run_id,
            verify_passed,
            verify_timed_out,
            failure_class,
            changed_files,
            symbol_drift_violations,
            close,
        } = req;

        let evidence = state
            .evidence_by_task
            .get(&task_id)
            .cloned()
            .unwrap_or_default();
        let has_pass = evidence.iter().any(EvidenceRecord::is_success);
        let verify_passed = verify_passed.unwrap_or(has_pass);
        let mode_blocks = Self::mode_blocks(state);
        let lease_id_for_release = lease_id.clone();

        if verify_passed && state.require_evidence_for_close {
            let verify_evidence = reliability::Verifier::ensure_evidence(&evidence, 1)
                .map_err(|err| Error::validation(err.to_string()))?;
            if mode_blocks && !verify_evidence.ok {
                return Err(Error::validation(verify_evidence.violations.join("; ")));
            }
        }

        let mut verifier_policy_violations = Vec::new();
        {
            let Some(task) = state.tasks.get(&task_id) else {
                return Err(Error::validation(format!(
                    "Unknown reliability task: {task_id}"
                )));
            };

            let scope_result = reliability::Verifier::audit_scope_with_constraints(
                &changed_files,
                &task.spec.constraints,
            )
            .map_err(|err| Error::validation(err.to_string()))?;
            if !scope_result.ok {
                verifier_policy_violations.extend(scope_result.violations);
            }

            if verify_timed_out {
                verifier_policy_violations.push(format!(
                    "verification timed out (configured timeout={}s)",
                    match &task.spec.verify {
                        reliability::VerifyPlan::Standard { timeout_sec, .. } => timeout_sec,
                    }
                ));
            }

            if state
                .symbol_drift_required_by_task
                .get(&task_id)
                .copied()
                .unwrap_or(false)
                && !symbol_drift_violations.is_empty()
            {
                verifier_policy_violations.push(format!(
                    "symbol/API drift detected: {}",
                    symbol_drift_violations.join("; ")
                ));
            }
        }
        if mode_blocks && !verifier_policy_violations.is_empty() {
            return Err(Error::validation(verifier_policy_violations.join("; ")));
        }

        let mut state_label = {
            let Some(task) = state.tasks.get_mut(&task_id) else {
                return Err(Error::validation(format!(
                    "Unknown reliability task: {task_id}"
                )));
            };
            reliability::apply_transition(
                &mut task.runtime,
                &reliability::TransitionEvent::Submit {
                    lease_id,
                    fence_token,
                    patch_digest: patch_digest.clone(),
                    verify_run_id: verify_run_id.clone(),
                },
                task.spec.max_attempts,
            )
            .map_err(|err| Error::validation(format!("submit transition rejected: {err}")))?;

            if verify_passed {
                reliability::apply_transition(
                    &mut task.runtime,
                    &reliability::TransitionEvent::VerifySuccess {
                        verify_run_id,
                        patch_digest,
                    },
                    task.spec.max_attempts,
                )
                .map_err(|err| {
                    Error::validation(format!("verify success transition rejected: {err}"))
                })?;
            } else {
                reliability::apply_transition(
                    &mut task.runtime,
                    &reliability::TransitionEvent::VerifyFail {
                        class: failure_class
                            .unwrap_or(reliability::FailureClass::VerificationFailed),
                        verify_run_id: Some(verify_run_id),
                        failure_artifact: None,
                        handoff_summary: "Verification failed during RPC submit_task".to_string(),
                    },
                    task.spec.max_attempts,
                )
                .map_err(|err| {
                    Error::validation(format!("verify fail transition rejected: {err}"))
                })?;
            }

            if !state.allow_open_ended_defer
                && matches!(
                    task.runtime.state,
                    reliability::RuntimeState::Recoverable { .. }
                )
            {
                reliability::RecoveryManager::set_retry_after(task, 60);
            }

            Self::state_label(&task.runtime.state).to_string()
        };
        let _ = state.leases.expire_lease(&lease_id_for_release);
        state.refresh_dependency_states();
        if let Some(task) = state.tasks.get(&task_id) {
            state_label = Self::state_label(&task.runtime.state).to_string();
        }

        let strict_close_trace_chain = mode_blocks && (verify_passed || close.is_some());
        let mut close_payload = close.unwrap_or_else(|| ClosePayload {
            task_id: task_id.clone(),
            outcome: "submitted via rpc".to_string(),
            outcome_kind: Some(reliability::CloseOutcomeKind::Success),
            acceptance_ids: if strict_close_trace_chain {
                Vec::new()
            } else {
                vec!["rpc:auto".to_string()]
            },
            evidence_ids: Vec::new(),
            trace_parent: if strict_close_trace_chain {
                None
            } else {
                Some("rpc:auto".to_string())
            },
        });
        if close_payload.task_id.trim().is_empty() {
            close_payload.task_id.clone_from(&task_id);
        }
        if close_payload
            .trace_parent
            .as_ref()
            .is_none_or(|trace| trace.trim().is_empty())
            && !strict_close_trace_chain
        {
            close_payload.trace_parent = Some("rpc:auto".to_string());
        }
        if close_payload.acceptance_ids.is_empty() && !strict_close_trace_chain {
            close_payload.acceptance_ids = vec!["rpc:auto".to_string()];
        }
        if close_payload.evidence_ids.is_empty() {
            close_payload.evidence_ids =
                evidence.iter().map(|rec| rec.evidence_id.clone()).collect();
        }
        if strict_close_trace_chain {
            Self::validate_close_trace_chain(state, &task_id, &close_payload)?;
        }

        let mapping = reliability::Verifier::ensure_acceptance_mapped(
            &close_payload.acceptance_ids,
            &close_payload.evidence_ids,
        );
        if mode_blocks && !mapping.ok {
            return Err(Error::validation(mapping.violations.join("; ")));
        }

        let mut close = reliability::CloseResult::evaluate(
            &close_payload,
            &evidence,
            state.require_evidence_for_close,
        )
        .map_err(|err| Error::validation(err.to_string()))?;
        if !mapping.ok {
            close.approved = false;
            close.violations.extend(mapping.violations);
        }
        if !verifier_policy_violations.is_empty() {
            close.approved = false;
            close.violations.extend(verifier_policy_violations);
        }

        if mode_blocks {
            close
                .ensure_approved()
                .map_err(|err| Error::validation(err.to_string()))?;
        }

        Ok(SubmitTaskResponse {
            task_id,
            state: state_label,
            close_payload,
            close,
        })
    }

    pub(crate) fn resolve_blocker(
        state: &mut RpcReliabilityState,
        report: crate::rpc::BlockerReport,
    ) -> Result<String> {
        state.ensure_enabled()?;
        let is_defer = report.reason.trim().eq_ignore_ascii_case("defer");
        let report_reason = report.reason.clone();
        let report_context = report.context.clone();
        let lease_id_for_release = report.lease_id.clone();
        if !report.resolved
            && is_defer
            && !state.allow_open_ended_defer
            && report.defer_trigger.is_none()
        {
            return Err(Error::validation(
                "Open-ended defer blocker is disabled by reliability policy; provide defer_trigger Until or DependsOn",
            ));
        }

        {
            let Some(task) = state.tasks.get_mut(&report.task_id) else {
                return Err(Error::validation(format!(
                    "Unknown reliability task: {}",
                    report.task_id
                )));
            };

            let event = if report.resolved {
                reliability::TransitionEvent::HumanResolve
            } else {
                reliability::TransitionEvent::ReportBlocker {
                    lease_id: report.lease_id,
                    fence_token: report.fence_token,
                    reason: report_reason,
                    context: report_context.clone(),
                }
            };
            reliability::apply_transition(&mut task.runtime, &event, task.spec.max_attempts)
                .map_err(|err| {
                    Error::validation(format!("resolve blocker transition rejected: {err}"))
                })?;

            if !report.resolved && is_defer {
                if let Some(trigger) = &report.defer_trigger {
                    match trigger {
                        reliability::DeferTrigger::Until { until } => {
                            task.runtime.state = reliability::RuntimeState::Recoverable {
                                reason: reliability::FailureClass::HumanBlocker,
                                failure_artifact: None,
                                handoff_summary: report_context,
                                retry_after: Some(*until),
                            };
                        }
                        reliability::DeferTrigger::DependsOn { task_id } => {
                            task.runtime.state = reliability::RuntimeState::Blocked {
                                waiting_on: vec![task_id.clone()],
                            };
                        }
                    }
                }
            }
        }

        if !report.resolved {
            let _ = state.leases.expire_lease(&lease_id_for_release);
        }

        Self::refresh_dependency_states(state);
        let Some(task) = state.tasks.get(&report.task_id) else {
            return Err(Error::validation(format!(
                "Unknown reliability task: {}",
                report.task_id
            )));
        };
        Ok(Self::state_label(&task.runtime.state).to_string())
    }

    pub(crate) fn query_artifact(
        state: &RpcReliabilityState,
        query: crate::rpc::ArtifactQuery,
    ) -> Result<Vec<String>> {
        state.ensure_enabled()?;
        state
            .artifacts
            .list(&query)
            .map_err(|err| Error::session(format!("artifact query failed: {err}")))
    }

    pub(crate) fn load_artifact_text(
        state: &RpcReliabilityState,
        artifact_id: &str,
    ) -> Result<String> {
        state.ensure_enabled()?;
        let bytes = state
            .artifacts
            .load(artifact_id)
            .map_err(|err| Error::session(format!("artifact load failed: {err}")))?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    pub(crate) fn get_state_digest_state(
        state: &mut RpcReliabilityState,
        task_id: &str,
    ) -> Result<StateDigest> {
        state.ensure_enabled()?;
        state.refresh_dependency_states();
        let Some(task) = state.tasks.get(task_id) else {
            return Err(Error::validation(format!(
                "Unknown reliability task: {task_id}"
            )));
        };

        let mut digest = StateDigest::new(
            task.spec.objective.clone(),
            Self::state_label(&task.runtime.state),
        );
        digest
            .recent_actions
            .push(format!("attempt={}", task.runtime.attempt));
        digest
            .recent_actions
            .push(format!("max_attempts={}", task.spec.max_attempts));

        match &task.runtime.state {
            reliability::RuntimeState::Blocked { waiting_on } => {
                digest.blockers.clone_from(waiting_on);
                digest.next_action = Some("Resolve blockers".to_string());
            }
            reliability::RuntimeState::AwaitingHuman { question, .. } => {
                digest.blockers.push(question.clone());
                digest.next_action = Some("Resolve human blocker".to_string());
            }
            reliability::RuntimeState::Recoverable { retry_after, .. } => {
                digest.next_action = Some(retry_after.as_ref().map_or_else(
                    || "Promote recoverable task".to_string(),
                    |ts| format!("Retry after {}", ts.to_rfc3339()),
                ));
            }
            reliability::RuntimeState::Ready => {
                digest.next_action = Some("Request dispatch".to_string());
            }
            reliability::RuntimeState::Leased { .. } => {
                digest.next_action = Some("Submit task for verification".to_string());
            }
            reliability::RuntimeState::Verifying { .. } => {
                digest.next_action = Some("Finalize verify result".to_string());
            }
            reliability::RuntimeState::Terminal(term) => {
                let terminal = match term {
                    reliability::TerminalState::Succeeded { .. } => "succeeded",
                    reliability::TerminalState::Failed { .. } => "failed",
                    reliability::TerminalState::Superseded { .. } => "superseded",
                    reliability::TerminalState::Canceled { .. } => "canceled",
                };
                digest.next_action = Some(format!("Terminal: {terminal}"));
            }
        }

        state
            .latest_digest_by_task
            .insert(task_id.to_string(), digest.clone());
        Ok(digest)
    }
}
