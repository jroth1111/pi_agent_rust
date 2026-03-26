use crate::error::{Error, Result};
use crate::error_hints;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(crate) fn normalize_command_type(command_type: &str) -> &str {
    match command_type {
        "follow-up" | "followUp" | "queue-follow-up" | "queueFollowUp" => "follow_up",
        "get-state" | "getState" => "get_state",
        "set-model" | "setModel" => "set_model",
        "set-steering-mode" | "setSteeringMode" => "set_steering_mode",
        "set-follow-up-mode" | "setFollowUpMode" => "set_follow_up_mode",
        "set-auto-compaction" | "setAutoCompaction" => "set_auto_compaction",
        "set-auto-retry" | "setAutoRetry" => "set_auto_retry",
        "reliability.requestDispatch" | "reliability_request_dispatch" => {
            "reliability.request_dispatch"
        }
        "reliability.appendEvidence" | "reliability_append_evidence" => {
            "reliability.append_evidence"
        }
        "reliability.submitTask" | "reliability_submit_task" => "reliability.submit_task",
        "reliability.resolveBlocker" | "reliability_resolve_blocker" => {
            "reliability.resolve_blocker"
        }
        "reliability.queryArtifact" | "reliability_query_artifact" => "reliability.query_artifact",
        "reliability.getStateDigest" | "reliability_get_state_digest" => {
            "reliability.get_state_digest"
        }
        "orchestration.startRun" | "orchestration_start_run" => "orchestration.start_run",
        "orchestration.getRun" | "orchestration_get_run" => "orchestration.get_run",
        "orchestration.dispatchRun" | "orchestration_dispatch_run" => "orchestration.dispatch_run",
        "orchestration.cancelRun" | "orchestration_cancel_run" => "orchestration.cancel_run",
        "orchestration.resumeRun" | "orchestration_resume_run" => "orchestration.resume_run",
        _ => command_type,
    }
}

pub(crate) fn command_payload(parsed: &Value) -> Value {
    parsed
        .get("payload")
        .cloned()
        .unwrap_or_else(|| parsed.clone())
}

pub(crate) fn parse_command_payload<T>(parsed: &Value, command_type: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(command_payload(parsed))
        .map_err(|err| Error::validation(format!("Invalid payload for {command_type}: {err}")))
}

pub(crate) fn response_ok(id: Option<String>, command: &str, data: Option<Value>) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": true,
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    if let Some(data) = data {
        resp["data"] = data;
    }
    resp.to_string()
}

pub(crate) fn response_error(
    id: Option<String>,
    command: &str,
    error: impl Into<String>,
) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": false,
        "error": error.into(),
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    resp.to_string()
}

pub(crate) fn response_error_with_hints(
    id: Option<String>,
    command: &str,
    error: &Error,
) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": false,
        "error": error.to_string(),
        "errorHints": error_hints_value(error),
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    resp.to_string()
}

pub(crate) fn error_hints_value(error: &Error) -> Value {
    let hint = error_hints::hints_for_error(error);
    json!({
        "summary": hint.summary,
        "hints": hint.hints,
        "contextFields": hint.context_fields,
    })
}
