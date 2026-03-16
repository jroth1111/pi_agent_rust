use crate::runtime::reliability::verifier::Verifier;
use crate::runtime::types::{
    RunPhase, RunSnapshot, RunVerifyScopeKind, RunVerifyStatus, TaskState,
};
use chrono::Utc;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedRunVerifyScope {
    pub scope_id: String,
    pub scope_kind: RunVerifyScopeKind,
    pub subrun_id: Option<String>,
}

pub fn completed_run_verify_scope(run: &RunSnapshot) -> Option<CompletedRunVerifyScope> {
    if run.run_verify_command().trim().is_empty() || !run_terminal_success(run) {
        return None;
    }
    Some(CompletedRunVerifyScope {
        scope_id: run.spec.run_id.clone(),
        scope_kind: RunVerifyScopeKind::Run,
        subrun_id: None,
    })
}

pub fn should_skip_run_verify(run: &RunSnapshot, scope: &CompletedRunVerifyScope) -> bool {
    run.dispatch
        .latest_run_verify
        .as_ref()
        .is_some_and(|status| {
            status.scope_id == scope.scope_id
                && status.scope_kind == scope.scope_kind
                && status.subrun_id == scope.subrun_id
        })
}

pub fn completed_scope_from_run_verify(status: &RunVerifyStatus) -> CompletedRunVerifyScope {
    CompletedRunVerifyScope {
        scope_id: status.scope_id.clone(),
        scope_kind: status.scope_kind,
        subrun_id: status.subrun_id.clone(),
    }
}

pub fn apply_run_verify_lifecycle(run: &mut RunSnapshot) {
    if !matches!(run.phase, RunPhase::Canceled)
        && run
            .dispatch
            .latest_run_verify
            .as_ref()
            .is_some_and(|status| !status.ok)
    {
        run.phase = RunPhase::Failed;
        run.touch();
    }
}

pub async fn execute_run_verification(
    cwd: &Path,
    run: &mut RunSnapshot,
    scope: &CompletedRunVerifyScope,
) {
    let timeout_sec = run.spec.run_verify_timeout_sec.unwrap_or(60);
    let verify_status =
        match Verifier::execute_verify_command(cwd, run.run_verify_command(), timeout_sec).await {
            Ok(execution) => {
                let outcome = Verifier::classify_execution(&execution);
                let details = if outcome.ok {
                    format!(
                        "exit_code={}, duration_ms={}",
                        execution.exit_code, execution.duration_ms
                    )
                } else {
                    outcome.violations.join("; ")
                };
                RunVerifyStatus {
                    scope_id: scope.scope_id.clone(),
                    scope_kind: scope.scope_kind,
                    subrun_id: scope.subrun_id.clone(),
                    command: execution.command,
                    timeout_sec: execution.timeout_sec,
                    exit_code: execution.exit_code,
                    ok: outcome.ok,
                    summary: run_verify_scope_summary(scope, outcome.ok, details),
                    duration_ms: execution.duration_ms,
                    generated_at: Utc::now(),
                }
            }
            Err(err) => RunVerifyStatus {
                scope_id: scope.scope_id.clone(),
                scope_kind: scope.scope_kind,
                subrun_id: scope.subrun_id.clone(),
                command: run.run_verify_command().to_string(),
                timeout_sec,
                exit_code: -1,
                ok: false,
                summary: run_verify_scope_summary(scope, false, err.to_string()),
                duration_ms: 0,
                generated_at: Utc::now(),
            },
        };

    run.dispatch.latest_run_verify = Some(verify_status);
    apply_run_verify_lifecycle(run);
    run.touch();
}

fn run_verify_scope_summary(
    scope: &CompletedRunVerifyScope,
    ok: bool,
    details: impl AsRef<str>,
) -> String {
    let scope_label = match scope.scope_kind {
        RunVerifyScopeKind::Run => "run",
        RunVerifyScopeKind::Wave => "wave",
        RunVerifyScopeKind::Subrun => "subrun",
    };
    let prefix = if ok {
        "run verification passed"
    } else {
        "run verification failed"
    };
    format!(
        "{prefix} for {scope_label} {}: {}",
        scope.scope_id,
        details.as_ref().trim()
    )
}

fn run_terminal_success(run: &RunSnapshot) -> bool {
    !run.tasks.is_empty()
        && run.tasks.values().all(|task| {
            matches!(
                task.runtime.state,
                TaskState::Succeeded | TaskState::Superseded
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::types::{
        AutonomyLevel, RunBudgets, RunConstraints, RunSpec, TaskConstraints, TaskNode, TaskSpec,
        VerifySpec,
    };
    use chrono::Utc;
    use std::path::PathBuf;

    fn sample_run() -> RunSnapshot {
        RunSnapshot::new(RunSpec {
            run_id: "run-1".to_string(),
            objective: "Verify runtime".to_string(),
            root_workspace: PathBuf::from("/tmp/pi"),
            policy_profile: "default".to_string(),
            model_profile: "default".to_string(),
            run_verify_command: Some("true".to_string()),
            run_verify_timeout_sec: Some(30),
            budgets: RunBudgets::default(),
            constraints: RunConstraints::default(),
            created_at: Utc::now(),
        })
    }

    fn task(task_id: &str, state: TaskState) -> TaskNode {
        let mut task = TaskNode::new(TaskSpec {
            task_id: task_id.to_string(),
            title: task_id.to_string(),
            objective: task_id.to_string(),
            parent_goal_trace_id: None,
            planned_touches: Vec::new(),
            input_snapshot: None,
            max_attempts: 1,
            enforce_symbol_drift_check: false,
            verify: VerifySpec {
                command: "true".to_string(),
                timeout_sec: 30,
                acceptance_ids: Vec::new(),
            },
            autonomy: AutonomyLevel::Guarded,
            constraints: TaskConstraints::default(),
        });
        task.runtime.state = state;
        task
    }

    #[test]
    fn completed_scope_requires_terminal_success() {
        let mut run = sample_run();
        run.tasks
            .insert("task-a".to_string(), task("task-a", TaskState::Succeeded));
        run.tasks
            .insert("task-b".to_string(), task("task-b", TaskState::Recoverable));

        assert!(completed_run_verify_scope(&run).is_none());
    }

    #[test]
    fn completed_scope_detects_successful_run() {
        let mut run = sample_run();
        run.tasks
            .insert("task-a".to_string(), task("task-a", TaskState::Succeeded));
        run.tasks
            .insert("task-b".to_string(), task("task-b", TaskState::Superseded));

        let scope = completed_run_verify_scope(&run).expect("scope");
        assert_eq!(scope.scope_id, "run-1");
        assert_eq!(scope.scope_kind, RunVerifyScopeKind::Run);
    }
}
