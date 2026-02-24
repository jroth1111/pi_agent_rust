//! ExecPolicy DSL for command execution control.
//!
//! This module implements a Codex-CLI style policy DSL for controlling
//! which commands are allowed or denied based on prefix matching.
//!
//! # Policy Rule Syntax
//!
//! Rules use prefix matching with first-match-wins semantics:
//!
//! ```toml
//! [[rules]]
//! prefix = "allow git"
//!
//! [[rules]]
//! prefix = "deny rm -rf"
//! options = { needs_confirmation = true }
//! ```
//!
//! # Example
//!
//! ```
//! use pi::policy::{ExecPolicy, PolicyRule, RuleOptions, CommandPolicy, PolicyDecision};
//!
//! let mut policy = ExecPolicy::new();
//! policy.add_rule(PolicyRule::allow("git"));
//! policy.add_rule(PolicyRule::deny("rm -rf"));
//!
//! assert!(policy.check("git status").is_allowed());
//! assert!(policy.check("rm -rf /").is_denied());
//! ```

use serde::{Deserialize, Serialize};

use super::{CommandPolicy, PolicyDecision, bash_ast::BashAstParser};

/// Options that modify rule behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuleOptions {
    /// Timeout in seconds for commands matching this rule.
    /// If `None`, uses the default timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,

    /// Whether this command requires user confirmation before execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_confirmation: Option<bool>,
}

impl RuleOptions {
    /// Creates new rule options with default values.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            timeout: None,
            needs_confirmation: None,
        }
    }

    /// Sets the timeout for this rule.
    #[must_use]
    pub const fn with_timeout(mut self, seconds: u64) -> Self {
        self.timeout = Some(seconds);
        self
    }

    /// Sets whether confirmation is needed.
    #[must_use]
    pub const fn with_confirmation(mut self, needed: bool) -> Self {
        self.needs_confirmation = Some(needed);
        self
    }
}

/// A single policy rule with a prefix and optional configuration.
///
/// Rules are matched against command strings using prefix matching.
/// The prefix format is: `"allow|deny <command-prefix>"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyRule {
    /// The prefix to match against commands.
    /// Format: "allow|deny <command-prefix>"
    /// Example: "allow git", "deny rm -rf"
    pub prefix: String,

    /// Optional configuration for this rule.
    #[serde(default, skip_serializing_if = "RuleOptions::is_default")]
    pub options: RuleOptions,
}

impl RuleOptions {
    /// Returns `true` if all options are at their default values.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        self.timeout.is_none() && self.needs_confirmation.is_none()
    }
}

impl PolicyRule {
    /// Creates a new policy rule.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            options: RuleOptions::new(),
        }
    }

    /// Creates an allow rule for the given command prefix.
    #[must_use]
    pub fn allow(cmd_prefix: impl Into<String>) -> Self {
        Self::new(format!("allow {}", cmd_prefix.into()))
    }

    /// Creates a deny rule for the given command prefix.
    #[must_use]
    pub fn deny(cmd_prefix: impl Into<String>) -> Self {
        Self::new(format!("deny {}", cmd_prefix.into()))
    }

    /// Adds options to this rule.
    #[must_use]
    pub const fn with_options(mut self, options: RuleOptions) -> Self {
        self.options = options;
        self
    }

    /// Parses the prefix to determine if this rule allows or denies.
    ///
    /// Returns `None` if the prefix format is invalid.
    fn parse_action(&self) -> Option<RuleAction> {
        let prefix = self.prefix.trim();
        if let Some(cmd) = prefix.strip_prefix("allow ") {
            Some(RuleAction::Allow(cmd.to_string()))
        } else if let Some(cmd) = prefix.strip_prefix("deny ") {
            Some(RuleAction::Deny(cmd.to_string()))
        } else if prefix == "allow" {
            Some(RuleAction::Allow(String::new()))
        } else if prefix == "deny" {
            Some(RuleAction::Deny(String::new()))
        } else {
            None
        }
    }

    /// Checks if this rule matches the given command.
    ///
    /// Returns `Some(PolicyDecision)` if the rule matches, `None` otherwise.
    pub fn matches(&self, cmd: &str) -> Option<PolicyDecision> {
        let action = self.parse_action()?;

        let matches = match &action {
            RuleAction::Allow(prefix) | RuleAction::Deny(prefix) => {
                if prefix.is_empty() {
                    // Empty prefix matches everything
                    true
                } else {
                    // Normalize both for comparison (trim leading whitespace from cmd)
                    let cmd_normalized = cmd.trim_start();
                    cmd_normalized.starts_with(prefix)
                }
            }
        };

        if matches {
            Some(match action {
                RuleAction::Allow(_) => PolicyDecision::allow(),
                RuleAction::Deny(prefix) => {
                    PolicyDecision::deny(format!("Command denied by policy rule: 'deny {prefix}'"))
                }
            })
        } else {
            None
        }
    }

    /// Checks if this rule matches an AST-parsed command.
    ///
    /// This provides more accurate matching than string prefix matching,
    /// especially for detecting dangerous patterns. It matches against the
    /// program name extracted from the AST.
    ///
    /// Returns `Some(PolicyDecision)` if the rule matches, `None` otherwise.
    pub fn matches_ast(&self, ast: &super::BashCommand) -> Option<PolicyDecision> {
        let action = self.parse_action()?;

        // Extract the program name for matching
        let program_name = ast.program_name();

        let matches = match &action {
            RuleAction::Allow(prefix) | RuleAction::Deny(prefix) => {
                if prefix.is_empty() {
                    // Empty prefix matches everything
                    true
                } else if let Some(prog) = program_name {
                    // Match against the program name
                    prog == prefix || prog.starts_with(&format!("{prefix} "))
                } else {
                    // No program name to match (e.g., just a variable expansion)
                    false
                }
            }
        };

        if matches {
            // Check for dangerous patterns and enhance the decision
            let danger_flags = ast.danger_flags();

            Some(match &action {
                RuleAction::Allow(_) => {
                    // Even if allowed, warn about dangerous patterns
                    if danger_flags.is_dangerous() {
                        PolicyDecision::deny(format!(
                            "Command contains dangerous patterns: {}",
                            danger_flags.describe()
                        ))
                    } else {
                        PolicyDecision::allow()
                    }
                }
                RuleAction::Deny(prefix) => {
                    PolicyDecision::deny(format!("Command denied by policy rule: 'deny {prefix}'"))
                }
            })
        } else {
            None
        }
    }
}

/// Internal representation of a rule's action.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RuleAction {
    Allow(String),
    Deny(String),
}

/// ExecPolicy implements a DSL for command execution control.
///
/// Policies are loaded from configuration and evaluated using
/// first-match-wins semantics.
///
/// # Example
///
/// ```
/// use pi::policy::ExecPolicy;
///
/// let policy = ExecPolicy::from_toml(r#"
/// [[rules]]
/// prefix = "allow git"
///
/// [[rules]]
/// prefix = "deny rm -rf"
///
/// [[rules]]
/// prefix = "allow"
/// "#).unwrap();
///
/// assert!(policy.check("git status").is_allowed());
/// assert!(policy.check("rm -rf /").is_denied());
/// assert!(policy.check("ls").is_allowed()); // Matches "allow"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecPolicy {
    /// The list of rules to evaluate in order.
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

impl ExecPolicy {
    /// Creates a new empty policy.
    #[must_use]
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Adds a rule to the policy.
    pub fn add_rule(&mut self, rule: PolicyRule) {
        self.rules.push(rule);
    }

    /// Creates a policy with the given rules.
    #[must_use]
    pub const fn with_rules(rules: Vec<PolicyRule>) -> Self {
        Self { rules }
    }

    /// Parses a TOML configuration string into a policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the TOML is malformed or contains invalid rules.
    pub fn from_toml(toml: &str) -> std::result::Result<Self, toml::de::Error> {
        toml::from_str(toml)
    }

    /// Parses a JSON configuration string into a policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is malformed or contains invalid rules.
    pub fn from_json(json: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Converts the policy to a TOML string.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_toml(&self) -> std::result::Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Converts the policy to a JSON string.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> std::result::Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Checks a command using AST-based parsing for more accurate decisions.
    ///
    /// This method first parses the command using the AST parser, then checks
    /// it against the policy rules. This provides more accurate detection of
    /// dangerous patterns compared to simple string matching.
    ///
    /// # Arguments
    ///
    /// * `cmd` - The command string to check.
    ///
    /// # Returns
    ///
    /// A `PolicyDecision` indicating whether the command is allowed or denied.
    pub fn check_ast(&self, cmd: &str) -> PolicyDecision {
        let parser = BashAstParser::new();

        match parser.parse(cmd) {
            Ok(ast) => {
                // Check if the command itself is dangerous
                if ast.is_dangerous() {
                    return PolicyDecision::deny(format!(
                        "Command contains dangerous patterns: {}",
                        ast.danger_flags().describe()
                    ));
                }

                // Check against policy rules using AST
                for rule in &self.rules {
                    if let Some(decision) = rule.matches_ast(&ast) {
                        return decision;
                    }
                }

                // No rule matched - default deny
                PolicyDecision::deny(format!("Command '{cmd}' did not match any policy rule"))
            }
            Err(_e) => {
                // Fall back to string-based matching if AST parsing fails
                self.check(cmd)
            }
        }
    }
}

impl CommandPolicy for ExecPolicy {
    fn check(&self, cmd: &str) -> PolicyDecision {
        for rule in &self.rules {
            if let Some(decision) = rule.matches(cmd) {
                return decision;
            }
        }
        // No rule matched - default deny
        PolicyDecision::deny(format!("Command '{cmd}' did not match any policy rule"))
    }

    fn name(&self) -> &'static str {
        "exec-policy"
    }
}

/// Configuration for loading ExecPolicy from files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecPolicyConfig {
    /// The exec policy.
    #[serde(default)]
    pub policy: ExecPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_options_default() {
        let opts = RuleOptions::new();
        assert!(opts.timeout.is_none());
        assert!(opts.needs_confirmation.is_none());
        assert!(opts.is_default());
    }

    #[test]
    fn test_rule_options_with_timeout() {
        let opts = RuleOptions::new().with_timeout(30);
        assert_eq!(opts.timeout, Some(30));
        assert!(!opts.is_default());
    }

    #[test]
    fn test_rule_options_with_confirmation() {
        let opts = RuleOptions::new().with_confirmation(true);
        assert_eq!(opts.needs_confirmation, Some(true));
        assert!(!opts.is_default());
    }

    #[test]
    fn test_policy_rule_allow() {
        let rule = PolicyRule::allow("git");
        assert_eq!(rule.prefix, "allow git");
    }

    #[test]
    fn test_policy_rule_deny() {
        let rule = PolicyRule::deny("rm -rf");
        assert_eq!(rule.prefix, "deny rm -rf");
    }

    #[test]
    fn test_rule_matches_allow() {
        let rule = PolicyRule::allow("git");
        assert!(rule.matches("git status").unwrap().is_allowed());
        assert!(rule.matches("git").unwrap().is_allowed());
        assert!(rule.matches("gitclone").unwrap().is_allowed()); // Prefix match
        assert!(rule.matches("ls").is_none());
    }

    #[test]
    fn test_rule_matches_deny() {
        let rule = PolicyRule::deny("rm -rf");
        assert!(rule.matches("rm -rf /").unwrap().is_denied());
        assert!(rule.matches("rm -rf").unwrap().is_denied());
        assert!(rule.matches("rm file").is_none());
        assert!(rule.matches("ls").is_none());
    }

    #[test]
    fn test_exec_policy_first_match_wins() {
        let mut policy = ExecPolicy::new();
        policy.add_rule(PolicyRule::allow("git"));
        policy.add_rule(PolicyRule::deny("git push --force"));

        // First rule matches, second never evaluated
        assert!(policy.check("git push --force").is_allowed());
        assert!(policy.check("git status").is_allowed());
    }

    #[test]
    fn test_exec_policy_default_deny() {
        let policy = ExecPolicy::new();
        assert!(policy.check("any-command").is_denied());
    }

    #[test]
    fn test_exec_policy_from_toml() {
        let toml = r#"
[[rules]]
prefix = "allow git"

[[rules]]
prefix = "deny rm -rf"

[[rules]]
prefix = "allow"
"#;
        let policy = ExecPolicy::from_toml(toml).expect("parse toml");
        assert_eq!(policy.rules.len(), 3);
        assert!(policy.check("git status").is_allowed());
        assert!(policy.check("rm -rf /").is_denied());
        assert!(policy.check("ls").is_allowed());
    }

    #[test]
    fn test_exec_policy_from_json() {
        let json = r#"{
            "rules": [
                {"prefix": "allow git"},
                {"prefix": "deny rm -rf"}
            ]
        }"#;
        let policy = ExecPolicy::from_json(json).expect("parse json");
        assert_eq!(policy.rules.len(), 2);
        assert!(policy.check("git status").is_allowed());
        assert!(policy.check("rm -rf /").is_denied());
    }

    #[test]
    fn test_exec_policy_roundtrip_toml() {
        let mut policy = ExecPolicy::new();
        policy.add_rule(PolicyRule::allow("git"));
        policy
            .add_rule(PolicyRule::deny("rm -rf").with_options(RuleOptions::new().with_timeout(10)));

        let toml = policy.to_toml().expect("serialize");
        let parsed = ExecPolicy::from_toml(&toml).expect("parse");
        assert_eq!(policy, parsed);
    }

    #[test]
    fn test_exec_policy_roundtrip_json() {
        let mut policy = ExecPolicy::new();
        policy.add_rule(PolicyRule::allow("git"));
        policy.add_rule(PolicyRule::deny("rm -rf"));

        let json = policy.to_json().expect("serialize");
        let parsed = ExecPolicy::from_json(&json).expect("parse");
        assert_eq!(policy, parsed);
    }

    #[test]
    fn test_exec_policy_with_options() {
        let toml = r#"
[[rules]]
prefix = "allow git"
options = { timeout = 30 }

[[rules]]
prefix = "deny rm"
options = { needs_confirmation = true, timeout = 5 }
"#;
        let policy = ExecPolicy::from_toml(toml).expect("parse toml");
        assert_eq!(policy.rules[0].options.timeout, Some(30));
        assert_eq!(policy.rules[1].options.needs_confirmation, Some(true));
        assert_eq!(policy.rules[1].options.timeout, Some(5));
    }

    #[test]
    fn test_empty_prefix_matches_all() {
        let allow_all = PolicyRule::new("allow");
        assert!(allow_all.matches("anything").unwrap().is_allowed());

        let deny_all = PolicyRule::new("deny");
        assert!(deny_all.matches("anything").unwrap().is_denied());
    }

    #[test]
    fn test_command_name() {
        let policy = ExecPolicy::new();
        assert_eq!(policy.name(), "exec-policy");
    }
}
