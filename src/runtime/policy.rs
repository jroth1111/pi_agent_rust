use crate::extensions::{
    DangerousCommandClass, ExecMediationPolicy, ExecMediationResult, evaluate_exec_mediation,
};
use crate::runtime::types::{RunId, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    Allow,
    AllowWithAudit,
    RequireApproval,
    Deny,
}

impl PolicyVerdict {
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow | Self::AllowWithAudit)
    }

    pub const fn requires_review(self) -> bool {
        matches!(self, Self::RequireApproval)
    }

    pub const fn is_denied(self) -> bool {
        matches!(self, Self::Deny)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PolicyTarget {
    Run {
        run_id: RunId,
        objective: String,
    },
    Task {
        run_id: RunId,
        task_id: TaskId,
        objective: String,
    },
    VerifyCommand {
        run_id: RunId,
        task_id: TaskId,
        command: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyRequest {
    pub target: PolicyTarget,
    #[serde(default)]
    pub planned_touches: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl PolicyRequest {
    pub fn new(target: PolicyTarget) -> Self {
        Self {
            target,
            planned_touches: Vec::new(),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyReason {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyDecision {
    pub verdict: PolicyVerdict,
    #[serde(default)]
    pub reasons: Vec<PolicyReason>,
}

impl PolicyDecision {
    pub const fn allow() -> Self {
        Self {
            verdict: PolicyVerdict::Allow,
            reasons: Vec::new(),
        }
    }

    pub fn review(reason: impl Into<String>) -> Self {
        Self {
            verdict: PolicyVerdict::RequireApproval,
            reasons: vec![PolicyReason {
                code: "review".to_string(),
                message: reason.into(),
            }],
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            verdict: PolicyVerdict::Deny,
            reasons: vec![PolicyReason {
                code: "deny".to_string(),
                message: reason.into(),
            }],
        }
    }

    pub fn with_reason(mut self, reason: PolicyReason) -> Self {
        self.reasons.push(reason);
        self
    }
}

pub trait RuntimePolicy: Send + Sync {
    fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision;
}

pub const BUILTIN_BASH_POLICY_SOURCE: &str = "builtin_bash_exec_mediation";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommandPolicyDecision {
    pub verdict: PolicyVerdict,
    pub command_class: Option<&'static str>,
    pub risk_tier: Option<&'static str>,
    pub interactive_class: Option<&'static str>,
    pub reason: Option<String>,
}

impl ShellCommandPolicyDecision {
    pub const fn decision_label(&self) -> &'static str {
        match self.verdict {
            PolicyVerdict::Allow => "allow",
            PolicyVerdict::AllowWithAudit => "allow_with_audit",
            PolicyVerdict::RequireApproval => "require_approval",
            PolicyVerdict::Deny => "deny",
        }
    }

    pub const fn is_denied(&self) -> bool {
        self.verdict.is_denied()
    }
}

fn classify_interactive_shell_command(command: &str) -> Option<&'static str> {
    let lower = command.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }

    let first_token = lower.split_whitespace().next().unwrap_or_default();
    if matches!(
        first_token,
        "vi" | "vim"
            | "nvim"
            | "nano"
            | "less"
            | "more"
            | "man"
            | "top"
            | "htop"
            | "watch"
            | "tmux"
            | "screen"
    ) {
        return Some("terminal_ui");
    }
    if first_token == "ssh" {
        return Some("remote_shell");
    }

    if lower.starts_with("tail -f")
        || lower.contains(" tail -f ")
        || lower.contains(" tail -f\t")
        || lower.contains(" --interactive")
        || lower.contains(" -it ")
        || lower.ends_with(" -it")
    {
        return Some("interactive_stream");
    }

    None
}

const fn class_details(class: DangerousCommandClass) -> (&'static str, &'static str) {
    (class.label(), class.risk_tier().label())
}

pub fn evaluate_shell_command_policy(
    policy: &ExecMediationPolicy,
    command: &str,
    hard_mode: bool,
) -> ShellCommandPolicyDecision {
    let mediation = evaluate_exec_mediation(policy, command, &[]);
    let interactive_class = if hard_mode {
        classify_interactive_shell_command(command)
    } else {
        None
    };

    let (verdict, command_class, risk_tier, reason) = match &mediation {
        ExecMediationResult::Allow => (PolicyVerdict::Allow, None, None, None),
        ExecMediationResult::AllowWithAudit { class, reason } => {
            let (label, tier) = class_details(*class);
            (
                PolicyVerdict::AllowWithAudit,
                Some(label),
                Some(tier),
                Some(reason.clone()),
            )
        }
        ExecMediationResult::Deny { class, reason } => {
            let (command_class, risk_tier) = class
                .map(class_details)
                .map_or((None, None), |(label, tier)| (Some(label), Some(tier)));
            (
                PolicyVerdict::Deny,
                command_class,
                risk_tier,
                Some(reason.clone()),
            )
        }
    };

    let reason = match &mediation {
        ExecMediationResult::Deny { reason, .. } => Some(reason.clone()),
        _ => interactive_class.map(|class| {
            format!("Command classified as interactive ({class}) and denied in hard reliability mode")
        }).or(reason),
    };

    ShellCommandPolicyDecision {
        verdict: if reason.is_some()
            && interactive_class.is_some()
            && !matches!(mediation, ExecMediationResult::Deny { .. })
        {
            PolicyVerdict::Deny
        } else {
            verdict
        },
        command_class,
        risk_tier,
        interactive_class,
        reason,
    }
}

#[derive(Default)]
pub struct PolicySet {
    policies: Vec<Box<dyn RuntimePolicy>>,
}

impl PolicySet {
    pub fn new() -> Self {
        Self {
            policies: Vec::new(),
        }
    }

    pub fn add_policy<P>(&mut self, policy: P)
    where
        P: RuntimePolicy + 'static,
    {
        self.policies.push(Box::new(policy));
    }

    pub fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision {
        let mut verdict = PolicyDecision::allow();
        for policy in &self.policies {
            let decision = policy.evaluate(request);
            if decision.verdict.is_denied() {
                return decision;
            }
            if decision.verdict.requires_review() {
                verdict = decision;
            } else if decision.verdict == PolicyVerdict::AllowWithAudit {
                verdict.verdict = PolicyVerdict::AllowWithAudit;
                verdict.reasons.extend(decision.reasons);
            }
        }
        verdict
    }
}

impl RuntimePolicy for PolicySet {
    fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision {
        self.evaluate(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MaxTouchesPolicy {
        limit: usize,
    }

    impl RuntimePolicy for MaxTouchesPolicy {
        fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision {
            if request.planned_touches.len() > self.limit {
                PolicyDecision::review("too many touched files")
            } else {
                PolicyDecision::allow()
            }
        }
    }

    struct DenyWordPolicy;

    impl RuntimePolicy for DenyWordPolicy {
        fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision {
            match &request.target {
                PolicyTarget::VerifyCommand { command, .. } if command.contains("rm -rf") => {
                    PolicyDecision::deny("dangerous verify command")
                }
                _ => PolicyDecision::allow(),
            }
        }
    }

    #[test]
    fn policy_set_escalates_to_review() {
        let mut set = PolicySet::new();
        set.add_policy(MaxTouchesPolicy { limit: 1 });
        let mut request = PolicyRequest::new(PolicyTarget::Run {
            run_id: "run-1".to_string(),
            objective: "ship".to_string(),
        });
        request.planned_touches = vec!["a".to_string(), "b".to_string()];
        let decision = set.evaluate(&request);
        assert_eq!(decision.verdict, PolicyVerdict::RequireApproval);
    }

    #[test]
    fn policy_set_denial_wins() {
        let mut set = PolicySet::new();
        set.add_policy(MaxTouchesPolicy { limit: 10 });
        set.add_policy(DenyWordPolicy);
        let request = PolicyRequest::new(PolicyTarget::VerifyCommand {
            run_id: "run-1".to_string(),
            task_id: "task-1".to_string(),
            command: "rm -rf /tmp".to_string(),
        });
        let decision = set.evaluate(&request);
        assert_eq!(decision.verdict, PolicyVerdict::Deny);
    }

    #[test]
    fn shell_command_policy_audits_permission_escalation() {
        let decision = evaluate_shell_command_policy(
            &ExecMediationPolicy::default(),
            "chmod 777 ./__pi_missing_target__",
            false,
        );
        assert_eq!(decision.verdict, PolicyVerdict::AllowWithAudit);
        assert_eq!(decision.command_class, Some("permission_escalation"));
        assert_eq!(decision.risk_tier, Some("high"));
    }

    #[test]
    fn shell_command_policy_denies_interactive_commands_in_hard_mode() {
        let decision = evaluate_shell_command_policy(
            &ExecMediationPolicy::default(),
            "tail -f /dev/null",
            true,
        );
        assert_eq!(decision.verdict, PolicyVerdict::Deny);
        assert_eq!(decision.interactive_class, Some("interactive_stream"));
        assert!(
            decision
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("interactive"))
        );
    }
}
