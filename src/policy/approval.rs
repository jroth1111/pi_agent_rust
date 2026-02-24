//! Human approval flow for high-risk operations (HumanLayer approach).
//!
//! Critical operations require explicit human approval before execution.
//! This module provides risk classification and approval request/response
//! types for operations that need human oversight.
//!
//! # Risk Levels
//!
//! - **Low**: Read-only operations, safe commands (auto-approved)
//! - **Medium**: File modifications, git operations (usually auto-approved)
//! - **High**: Destructive operations, deployments (requires approval)
//! - **Critical**: Irreversible operations, production changes (always requires approval)
//!
//! # Example
//!
//! ```
//! use pi::policy::approval::{classify_risk, requires_approval, RiskLevel};
//!
//! let risk = classify_risk("git push --force origin main", false);
//! assert_eq!(risk, RiskLevel::Critical);
//! assert!(requires_approval(risk));
//! ```

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Risk level for operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum RiskLevel {
    /// Low risk: read-only operations, safe commands.
    /// These are typically auto-approved.
    #[default]
    Low,

    /// Medium risk: file modifications, git operations.
    /// Usually auto-approved but may prompt for confirmation.
    Medium,

    /// High risk: destructive operations, deployments.
    /// Requires explicit approval.
    High,

    /// Critical risk: irreversible operations, production changes.
    /// Always requires explicit approval with justification review.
    Critical,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

impl RiskLevel {
    /// Check if this risk level requires human approval.
    #[must_use]
    pub const fn needs_approval(&self) -> bool {
        matches!(self, Self::High | Self::Critical)
    }

    /// Get a human-readable description of the risk level.
    #[must_use]
    pub const fn description(&self) -> &'static str {
        match self {
            Self::Low => "Safe operation with minimal impact",
            Self::Medium => "Operation that modifies files or state",
            Self::High => "Destructive operation that requires approval",
            Self::Critical => "Irreversible operation requiring careful review",
        }
    }
}

/// Request for human approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Unique request ID for tracking.
    #[serde(default = "generate_request_id")]
    pub id: String,

    /// Operation description (human-readable).
    pub operation: String,

    /// Risk level classification.
    pub risk_level: RiskLevel,

    /// Why this operation is needed.
    pub justification: String,

    /// The command or action to be performed.
    pub action: String,

    /// Working directory where the action will be performed.
    #[serde(default)]
    pub working_directory: Option<String>,

    /// Auto-approve timeout (None = never auto-approve).
    #[serde(with = "serde_duration_opt")]
    pub auto_approve_after: Option<Duration>,

    /// Timestamp when the request was created.
    #[serde(default = "chrono::Utc::now")]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

fn generate_request_id() -> String {
    format!("appr-{}", uuid::Uuid::new_v4().simple())
}

mod serde_duration_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match duration {
            Some(d) => serializer.serialize_some(&d.as_secs()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<u64>::deserialize(deserializer)?;
        Ok(opt.map(Duration::from_secs))
    }
}

impl ApprovalRequest {
    /// Create a new approval request.
    pub fn new(
        operation: impl Into<String>,
        action: impl Into<String>,
        risk_level: RiskLevel,
    ) -> Self {
        Self {
            id: generate_request_id(),
            operation: operation.into(),
            risk_level,
            justification: String::new(),
            action: action.into(),
            working_directory: None,
            auto_approve_after: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// Add a justification for the operation.
    #[must_use]
    pub fn with_justification(mut self, justification: impl Into<String>) -> Self {
        self.justification = justification.into();
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_working_directory(mut self, dir: impl Into<String>) -> Self {
        self.working_directory = Some(dir.into());
        self
    }

    /// Set auto-approve timeout.
    #[must_use]
    pub const fn with_auto_approve(mut self, after: Duration) -> Self {
        self.auto_approve_after = Some(after);
        self
    }
}

/// Response to an approval request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    /// ID of the request being responded to.
    pub request_id: String,

    /// Whether the operation was approved.
    pub approved: bool,

    /// Optional feedback from the approver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,

    /// Who approved/denied (if applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,

    /// Timestamp of the response.
    #[serde(default = "chrono::Utc::now")]
    pub responded_at: chrono::DateTime<chrono::Utc>,
}

impl ApprovalResponse {
    /// Create an approval response.
    pub fn new(request_id: impl Into<String>, approved: bool) -> Self {
        Self {
            request_id: request_id.into(),
            approved,
            feedback: None,
            approver: None,
            responded_at: chrono::Utc::now(),
        }
    }

    /// Add feedback to the response.
    #[must_use]
    pub fn with_feedback(mut self, feedback: impl Into<String>) -> Self {
        self.feedback = Some(feedback.into());
        self
    }

    /// Set who provided the approval.
    #[must_use]
    pub fn with_approver(mut self, approver: impl Into<String>) -> Self {
        self.approver = Some(approver.into());
        self
    }

    /// Create an approved response.
    pub fn approved(request_id: impl Into<String>) -> Self {
        Self::new(request_id, true)
    }

    /// Create a denied response.
    pub fn denied(request_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::new(request_id, false).with_feedback(reason)
    }
}

/// Check if an operation requires human approval.
#[must_use]
pub const fn requires_approval(risk_level: RiskLevel) -> bool {
    risk_level.needs_approval()
}

/// Determine risk level from command and context.
///
/// This function analyzes a command string to determine its risk level.
/// It looks for known dangerous patterns and classifies accordingly.
///
/// # Arguments
///
/// * `command` - The command to classify
/// * `has_file_modifications` - Whether the operation modifies files
///
/// # Example
///
/// ```
/// use pi::policy::approval::{classify_risk, RiskLevel};
///
/// let risk = classify_risk("git status", false);
/// assert_eq!(risk, RiskLevel::Low);
///
/// let risk = classify_risk("rm -rf /", false);
/// assert_eq!(risk, RiskLevel::Critical);
/// ```
#[must_use]
pub fn classify_risk(command: &str, has_file_modifications: bool) -> RiskLevel {
    let cmd_lower = command.to_lowercase();
    let cmd_trimmed = cmd_lower.trim();

    // Critical patterns - irreversible operations
    let critical_patterns = [
        "rm -rf",
        "rm -r -f",
        "rmdir /s",
        "format",
        "git push --force",
        "git push -f",
        "drop table",
        "delete from",
        "truncate table",
        "shutdown",
        "reboot",
        "kubectl delete",
        "terraform destroy",
        "ansible-playbook",
    ];

    for pattern in &critical_patterns {
        if cmd_trimmed.contains(pattern) {
            return RiskLevel::Critical;
        }
    }

    // Check for recursive delete without force (still dangerous)
    if cmd_trimmed.contains("rm -r") && !cmd_trimmed.contains("-rf") {
        return RiskLevel::Critical;
    }

    // High risk patterns
    let high_patterns = [
        "git push",
        "git reset --hard",
        "deploy",
        "publish",
        "release",
        "npm publish",
        "cargo publish",
        "docker push",
        "kubectl apply",
        "helm upgrade",
        "terraform apply",
    ];

    for pattern in &high_patterns {
        if cmd_trimmed.contains(pattern) {
            return RiskLevel::High;
        }
    }

    // High risk if modifying system files
    if cmd_trimmed.starts_with("sudo") || cmd_trimmed.contains("/etc/") {
        return RiskLevel::High;
    }

    // Medium risk if modifying files
    if has_file_modifications {
        return RiskLevel::Medium;
    }

    // Medium risk for file write operations
    let medium_patterns = [
        "mv ",
        "cp ",
        "chmod",
        "chown",
        "git commit",
        "git merge",
        "git rebase",
        "npm install",
        "yarn add",
        "cargo add",
    ];

    for pattern in &medium_patterns {
        if cmd_trimmed.contains(pattern) {
            return RiskLevel::Medium;
        }
    }

    // Default to low for read-only operations
    RiskLevel::Low
}

/// Get suggested auto-approve timeout based on risk level.
#[must_use]
pub const fn suggested_auto_approve_timeout(risk_level: RiskLevel) -> Option<Duration> {
    match risk_level {
        RiskLevel::Low => Some(Duration::from_secs(0)), // Immediate auto-approve
        RiskLevel::Medium => Some(Duration::from_secs(30)), // 30 second timeout
        RiskLevel::High | RiskLevel::Critical => None,  // Never auto-approve
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_risk_level_needs_approval() {
        assert!(!RiskLevel::Low.needs_approval());
        assert!(!RiskLevel::Medium.needs_approval());
        assert!(RiskLevel::High.needs_approval());
        assert!(RiskLevel::Critical.needs_approval());
    }

    #[test]
    fn test_requires_approval_function() {
        assert!(!requires_approval(RiskLevel::Low));
        assert!(!requires_approval(RiskLevel::Medium));
        assert!(requires_approval(RiskLevel::High));
        assert!(requires_approval(RiskLevel::Critical));
    }

    #[test]
    fn test_classify_risk_critical() {
        assert_eq!(classify_risk("rm -rf /", false), RiskLevel::Critical);
        assert_eq!(
            classify_risk("rm -rf ./node_modules", false),
            RiskLevel::Critical
        );
        assert_eq!(
            classify_risk("git push --force origin main", false),
            RiskLevel::Critical
        );
        assert_eq!(
            classify_risk("DROP TABLE users;", false),
            RiskLevel::Critical
        );
    }

    #[test]
    fn test_classify_risk_high() {
        assert_eq!(
            classify_risk("git push origin main", false),
            RiskLevel::High
        );
        assert_eq!(classify_risk("npm publish", false), RiskLevel::High);
        assert_eq!(
            classify_risk("sudo apt install something", false),
            RiskLevel::High
        );
    }

    #[test]
    fn test_classify_risk_medium() {
        assert_eq!(
            classify_risk("git commit -m 'test'", false),
            RiskLevel::Medium
        );
        assert_eq!(classify_risk("npm install", false), RiskLevel::Medium);
        assert_eq!(classify_risk("mv file1 file2", false), RiskLevel::Medium);
    }

    #[test]
    fn test_classify_risk_low() {
        assert_eq!(classify_risk("ls -la", false), RiskLevel::Low);
        assert_eq!(classify_risk("git status", false), RiskLevel::Low);
        assert_eq!(classify_risk("cat file.txt", false), RiskLevel::Low);
    }

    #[test]
    fn test_classify_risk_with_file_modifications() {
        assert_eq!(classify_risk("some-command", true), RiskLevel::Medium);
    }

    #[test]
    fn test_approval_request_builder() {
        let req = ApprovalRequest::new(
            "Deploy to production",
            "kubectl apply -f deployment.yaml",
            RiskLevel::Critical,
        )
        .with_justification("Bug fix release")
        .with_working_directory("/project");

        assert!(!req.id.is_empty());
        assert_eq!(req.operation, "Deploy to production");
        assert_eq!(req.risk_level, RiskLevel::Critical);
        assert_eq!(req.justification, "Bug fix release");
        assert_eq!(req.working_directory, Some("/project".to_string()));
    }

    #[test]
    fn test_approval_response() {
        let resp = ApprovalResponse::approved("req-123");
        assert!(resp.approved);
        assert_eq!(resp.request_id, "req-123");

        let resp = ApprovalResponse::denied("req-456", "Too risky");
        assert!(!resp.approved);
        assert_eq!(resp.feedback, Some("Too risky".to_string()));
    }

    #[test]
    fn test_suggested_auto_approve_timeout() {
        assert_eq!(
            suggested_auto_approve_timeout(RiskLevel::Low),
            Some(Duration::from_secs(0))
        );
        assert_eq!(
            suggested_auto_approve_timeout(RiskLevel::Medium),
            Some(Duration::from_secs(30))
        );
        assert_eq!(suggested_auto_approve_timeout(RiskLevel::High), None);
        assert_eq!(suggested_auto_approve_timeout(RiskLevel::Critical), None);
    }

    #[test]
    fn test_risk_level_display() {
        assert_eq!(RiskLevel::Low.to_string(), "low");
        assert_eq!(RiskLevel::Medium.to_string(), "medium");
        assert_eq!(RiskLevel::High.to_string(), "high");
        assert_eq!(RiskLevel::Critical.to_string(), "critical");
    }

    #[test]
    fn test_risk_level_serde() {
        let level = RiskLevel::Critical;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, "\"critical\"");
        let parsed: RiskLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, level);
    }
}
