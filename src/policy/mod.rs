//! Policy system for command execution and file modification constraints.
//!
//! This module provides:
//! - `CommandPolicy` trait for checking command execution permissions
//! - `PolicyDecision` enum representing allow/deny outcomes
//! - `PolicySet` for combining multiple policies
//!
//! # Example
//!
//! ```
//! use pi::policy::{CommandPolicy, PolicyDecision, PolicySet, AlwaysAllowPolicy};
//!
//! let policy = AlwaysAllowPolicy;
//! let decision = policy.check("ls -la");
//! assert!(decision.is_allowed());
//! ```

mod approval;
mod bash_ast;
mod blocklist;
mod constraint;
mod edit_validation;
mod exec_policy;

pub use approval::{
    ApprovalRequest, ApprovalResponse, RiskLevel as ApprovalRiskLevel, classify_risk,
    requires_approval, suggested_auto_approve_timeout,
};
pub use bash_ast::{BashAstParser, BashCommand, DangerFlags, ParseError};
pub use blocklist::{
    CommandBlocklist, RiskAssessment, RiskLevel, assess_command_risk,
    assess_command_risk as assess_command_security_risk,
};
pub use constraint::{ConstraintSet, DiffSummary, FileOperation, Invariant};
pub use edit_validation::{
    BlockAnchorValidator, ContextAwareValidator, EditResult, EditStrategy, EditValidationChain,
    EditValidator, LevenshteinValidator,
};
pub use exec_policy::{ExecPolicy, PolicyRule, RuleOptions};

/// Decision outcome from a policy check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Command is allowed to execute.
    Allowed,
    /// Command is denied with a reason.
    Denied {
        /// Human-readable explanation for why the command was denied.
        reason: String,
    },
}

impl Default for PolicyDecision {
    fn default() -> Self {
        Self::deny("No policy specified")
    }
}

impl PolicyDecision {
    /// Returns `true` if this decision allows execution.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Returns `true` if this decision denies execution.
    #[must_use]
    pub const fn is_denied(&self) -> bool {
        matches!(self, Self::Denied { .. })
    }

    /// Returns the denial reason if this is a denied decision.
    #[must_use]
    pub fn denial_reason(&self) -> Option<&str> {
        match self {
            Self::Denied { reason } => Some(reason),
            Self::Allowed => None,
        }
    }

    /// Creates an allowed decision.
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allowed
    }

    /// Creates a denied decision with the given reason.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }
}

/// Trait for checking whether a command is allowed to execute.
///
/// Implementations define policy rules for command execution.
/// Policies are combined in a `PolicySet` for layered security.
pub trait CommandPolicy: Send + Sync {
    /// Check whether the given command is allowed.
    ///
    /// # Arguments
    ///
    /// * `cmd` - The command string to check (e.g., "rm -rf /tmp")
    ///
    /// # Returns
    ///
    /// A `PolicyDecision` indicating whether the command is allowed or denied.
    fn check(&self, cmd: &str) -> PolicyDecision;

    /// Returns a human-readable name for this policy.
    fn name(&self) -> &str;
}

/// A policy that always allows any command.
///
/// Useful as a default policy or for testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysAllowPolicy;

impl CommandPolicy for AlwaysAllowPolicy {
    fn check(&self, _cmd: &str) -> PolicyDecision {
        PolicyDecision::allow()
    }

    fn name(&self) -> &'static str {
        "always-allow"
    }
}

/// A policy that always denies any command.
///
/// Useful as a fail-safe default or for testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysDenyPolicy;

impl CommandPolicy for AlwaysDenyPolicy {
    fn check(&self, _cmd: &str) -> PolicyDecision {
        PolicyDecision::deny("All commands are denied by policy")
    }

    fn name(&self) -> &'static str {
        "always-deny"
    }
}

/// A set of policies that are checked in order.
///
/// Policies are checked in registration order. The first policy
/// that returns a definitive decision (deny or explicit allow)
/// determines the outcome.
///
/// # Example
///
/// ```
/// use pi::policy::{PolicySet, ExecPolicy, AlwaysDenyPolicy};
///
/// let mut set = PolicySet::new();
/// set.add_policy(ExecPolicy::default());
/// set.add_default_deny();
///
/// let decision = set.check("some-command");
/// ```
pub struct PolicySet {
    policies: Vec<Box<dyn CommandPolicy>>,
    default_decision: PolicyDecision,
}

impl Default for PolicySet {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicySet {
    /// Creates a new empty policy set with deny-by-default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            policies: Vec::new(),
            default_decision: PolicyDecision::deny("No policy matched"),
        }
    }

    /// Creates a policy set that allows all commands by default.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            policies: Vec::new(),
            default_decision: PolicyDecision::allow(),
        }
    }

    /// Adds a policy to the set.
    ///
    /// Policies are checked in the order they are added.
    pub fn add_policy<P: CommandPolicy + 'static>(&mut self, policy: P) {
        self.policies.push(Box::new(policy));
    }

    /// Sets the default decision when no policy matches.
    pub fn set_default(&mut self, decision: PolicyDecision) {
        self.default_decision = decision;
    }

    /// Adds an always-deny policy as the final fallback.
    pub fn add_default_deny(&mut self) {
        self.default_decision = PolicyDecision::deny("No policy allowed this command");
    }

    /// Checks a command against all policies.
    ///
    /// Policies are checked in order. The first non-neutral result
    /// determines the outcome. If no policy matches, the default
    /// decision is returned.
    pub fn check(&self, cmd: &str) -> PolicyDecision {
        if let Some(policy) = self.policies.first() {
            let decision = policy.check(cmd);
            // First-match-wins: return immediately on any decision
            return decision;
        }
        self.default_decision.clone()
    }

    /// Returns the number of policies in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.policies.len()
    }

    /// Returns `true` if there are no policies in the set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_policy_decision_allowed() {
        let decision = PolicyDecision::allow();
        assert!(decision.is_allowed());
        assert!(!decision.is_denied());
        assert!(decision.denial_reason().is_none());
    }

    #[test]
    fn test_policy_decision_denied() {
        let decision = PolicyDecision::deny("Test denial");
        assert!(!decision.is_allowed());
        assert!(decision.is_denied());
        assert_eq!(decision.denial_reason(), Some("Test denial"));
    }

    #[test]
    fn test_always_allow_policy() {
        let policy = AlwaysAllowPolicy;
        assert!(policy.check("rm -rf /").is_allowed());
        assert!(policy.check("dangerous-command").is_allowed());
        assert_eq!(policy.name(), "always-allow");
    }

    #[test]
    fn test_always_deny_policy() {
        let policy = AlwaysDenyPolicy;
        assert!(policy.check("ls").is_denied());
        assert!(policy.check("echo hello").is_denied());
        assert_eq!(policy.name(), "always-deny");
    }

    #[test]
    fn test_policy_set_default_deny() {
        let set = PolicySet::new();
        assert!(set.check("any-command").is_denied());
    }

    #[test]
    fn test_policy_set_with_policy() {
        let mut set = PolicySet::new();
        set.add_policy(AlwaysAllowPolicy);
        assert!(set.check("any-command").is_allowed());
    }

    #[test]
    fn test_policy_set_first_match_wins() {
        let mut set = PolicySet::new();
        set.add_policy(AlwaysDenyPolicy);
        set.add_policy(AlwaysAllowPolicy); // Never reached
        assert!(set.check("any-command").is_denied());
    }

    #[test]
    fn test_policy_set_len() {
        let mut set = PolicySet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);

        set.add_policy(AlwaysAllowPolicy);
        assert!(!set.is_empty());
        assert_eq!(set.len(), 1);
    }
}
