use crate::runtime::reliability::evidence::EvidenceRecord;
use crate::runtime::reliability::task::TaskConstraintSet;
use crate::tools::run_bash_command;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeAuditRule {
    pub max_touched_files: usize,
    #[serde(default)]
    pub allowed_prefixes: Vec<String>,
    #[serde(default)]
    pub forbidden_prefixes: Vec<String>,
}

impl Default for ScopeAuditRule {
    fn default() -> Self {
        Self {
            max_touched_files: 8,
            allowed_prefixes: Vec::new(),
            forbidden_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub ok: bool,
    #[serde(default)]
    pub violations: Vec<String>,
}

impl VerificationResult {
    pub const fn pass() -> Self {
        Self {
            ok: true,
            violations: Vec::new(),
        }
    }

    pub const fn fail(violations: Vec<String>) -> Self {
        Self {
            ok: false,
            violations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyExecution {
    pub command: String,
    pub timeout_sec: u32,
    pub exit_code: i32,
    pub cancelled: bool,
    pub output: String,
    pub duration_ms: u64,
    pub sandboxed: bool,
    pub sandbox_wrapper: Option<String>,
}

impl VerifyExecution {
    pub const fn passed(&self) -> bool {
        self.exit_code == 0 && !self.cancelled
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error("verification inputs are invalid: {0}")]
    InvalidInput(String),
    #[error("verification command failed to execute: {0}")]
    CommandExecution(String),
}

pub struct Verifier;

impl Verifier {
    pub fn audit_scope(
        changed_files: &[String],
        rule: &ScopeAuditRule,
    ) -> Result<VerificationResult, VerificationError> {
        if rule.max_touched_files == 0 {
            return Err(VerificationError::InvalidInput(
                "max_touched_files must be greater than zero".to_string(),
            ));
        }

        let mut violations = Vec::new();
        let unique_count = changed_files.iter().collect::<HashSet<_>>().len();
        if unique_count > rule.max_touched_files {
            violations.push(format!(
                "touched file count {unique_count} exceeds limit {}",
                rule.max_touched_files
            ));
        }

        for file in changed_files {
            if !rule.allowed_prefixes.is_empty()
                && !rule
                    .allowed_prefixes
                    .iter()
                    .any(|prefix| file.starts_with(prefix))
            {
                violations.push(format!("file outside allowed scope: {file}"));
            }
            if rule
                .forbidden_prefixes
                .iter()
                .any(|prefix| file.starts_with(prefix))
            {
                violations.push(format!("file touches forbidden path: {file}"));
            }
        }

        if violations.is_empty() {
            Ok(VerificationResult::pass())
        } else {
            Ok(VerificationResult::fail(violations))
        }
    }

    pub fn ensure_evidence(
        evidence: &[EvidenceRecord],
        min_passing: usize,
    ) -> Result<VerificationResult, VerificationError> {
        if min_passing == 0 {
            return Err(VerificationError::InvalidInput(
                "min_passing must be greater than zero".to_string(),
            ));
        }
        let pass_count = evidence.iter().filter(|e| e.is_success()).count();
        if pass_count >= min_passing {
            Ok(VerificationResult::pass())
        } else {
            Ok(VerificationResult::fail(vec![format!(
                "insufficient passing evidence: required {min_passing}, got {pass_count}"
            )]))
        }
    }

    pub fn ensure_acceptance_mapped(
        acceptance_ids: &[String],
        evidence_ids: &[String],
    ) -> VerificationResult {
        let mut violations = Vec::new();
        if acceptance_ids.is_empty() {
            violations.push("no acceptance IDs provided".to_string());
        }
        if evidence_ids.is_empty() {
            violations.push("no evidence IDs provided".to_string());
        }
        if acceptance_ids.len() > evidence_ids.len() {
            violations.push(format!(
                "acceptance to evidence mapping incomplete: {} acceptance IDs vs {} evidence IDs",
                acceptance_ids.len(),
                evidence_ids.len()
            ));
        }
        if violations.is_empty() {
            VerificationResult::pass()
        } else {
            VerificationResult::fail(violations)
        }
    }

    pub fn audit_scope_with_constraints(
        changed_files: &[String],
        constraints: &TaskConstraintSet,
    ) -> Result<VerificationResult, VerificationError> {
        let rule = ScopeAuditRule {
            max_touched_files: constraints
                .max_touched_files
                .map_or(usize::MAX, usize::from),
            allowed_prefixes: Vec::new(),
            forbidden_prefixes: constraints.forbid_paths.clone(),
        };
        Self::audit_scope(changed_files, &rule)
    }

    pub async fn execute_verify_command(
        cwd: &Path,
        command: &str,
        timeout_sec: u32,
    ) -> Result<VerifyExecution, VerificationError> {
        if command.trim().is_empty() {
            return Err(VerificationError::InvalidInput(
                "verify command cannot be empty".to_string(),
            ));
        }
        if timeout_sec == 0 {
            return Err(VerificationError::InvalidInput(
                "verify timeout must be greater than zero".to_string(),
            ));
        }

        let started = Instant::now();
        let bash = run_bash_command(cwd, None, None, command, Some(u64::from(timeout_sec)), None)
            .await
            .map_err(|err| VerificationError::CommandExecution(err.to_string()))?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        Ok(VerifyExecution {
            command: command.to_string(),
            timeout_sec,
            exit_code: bash.exit_code,
            cancelled: bash.cancelled,
            output: bash.output,
            duration_ms,
            sandboxed: bash.sandboxed,
            sandbox_wrapper: bash.sandbox_wrapper,
        })
    }

    pub fn classify_execution(execution: &VerifyExecution) -> VerificationResult {
        let mut violations = Vec::new();
        if execution.cancelled {
            violations.push(format!(
                "verification timed out or was cancelled (timeout={}s)",
                execution.timeout_sec
            ));
        }
        if execution.exit_code != 0 {
            violations.push(format!(
                "verification command exited with non-zero status: {}",
                execution.exit_code
            ));
        }
        if violations.is_empty() {
            VerificationResult::pass()
        } else {
            VerificationResult::fail(violations)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::reliability::EvidenceRecord;
    use crate::runtime::reliability::task::{NetworkPolicy, TaskConstraintSet};
    use std::path::Path;

    #[test]
    fn verifier_audit_scope_enforces_file_limit_and_forbidden_prefix() {
        let rule = ScopeAuditRule {
            max_touched_files: 1,
            allowed_prefixes: vec!["src/".to_string()],
            forbidden_prefixes: vec!["src/secrets/".to_string()],
        };
        let changed = vec!["src/main.rs".to_string(), "src/secrets/key.rs".to_string()];
        let result = Verifier::audit_scope(&changed, &rule).expect("audit");
        assert!(!result.ok);
        assert_eq!(result.violations.len(), 2);
    }

    #[test]
    fn verifier_requires_passing_evidence() {
        let failed = EvidenceRecord::from_command_output("t1", "cargo test", 1, "", "err", vec![]);
        let result = Verifier::ensure_evidence(&[failed], 1).expect("verify");
        assert!(!result.ok);
    }

    #[test]
    fn verifier_acceptance_mapping_detects_gap() {
        let result = Verifier::ensure_acceptance_mapped(
            &["a1".to_string(), "a2".to_string()],
            &["e1".to_string()],
        );
        assert!(!result.ok);
    }

    #[test]
    fn verifier_audit_scope_with_constraints_enforces_forbidden_paths() {
        let constraints = TaskConstraintSet {
            invariants: vec!["do not touch secrets".to_string()],
            max_touched_files: Some(3),
            forbid_paths: vec!["src/secrets/".to_string()],
            network_access: NetworkPolicy::Offline,
        };
        let changed = vec!["src/secrets/token.rs".to_string()];
        let result = Verifier::audit_scope_with_constraints(&changed, &constraints).expect("audit");
        assert!(!result.ok);
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn verifier_execute_verify_command_passes_for_successful_command() {
        let run = futures::executor::block_on(async {
            Verifier::execute_verify_command(Path::new("."), "echo verifier-ok", 5).await
        })
        .expect("verify run");
        assert!(run.passed());
        let verdict = Verifier::classify_execution(&run);
        assert!(verdict.ok);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn verifier_execute_verify_command_enforces_timeout_path() {
        let run = futures::executor::block_on(async {
            Verifier::execute_verify_command(Path::new("."), "sleep 2", 1).await
        })
        .expect("verify run");
        let verdict = Verifier::classify_execution(&run);
        assert!(!verdict.ok);
        assert!(
            run.cancelled || run.exit_code != 0,
            "timeout command should be cancelled or non-zero"
        );
    }
}
