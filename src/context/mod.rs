//! Context management for message visibility, filtering, and compaction.
//!
//! This module provides:
//! - Visibility control for messages, determining which messages appear in
//!   UI history vs LLM context
//! - Structured summaries for context compaction (OpenHands approach)
//! - Turn-based boundaries for atomic conversation units (Codex-CLI approach)
//! - History processor chain for modular message transformation (SWE-agent approach)
//! - Tool output pruning to reduce context window usage (OpenCode + SWE-agent)
//! - Cache control for prompt caching optimization (SWE-agent + Aider)
//! - Read deduplication to prevent redundant file content (Cline approach)
//! - Output thinning for large tool outputs (g3 approach)

pub mod cache_control;
pub mod dedup;
mod processors;
mod summary;
pub mod thinning;
pub mod threshold;
mod tool_output_pruner;
mod turns;
pub mod visibility;

pub use cache_control::{
    CACHE_CONTROL_KEY, CacheControl, CacheControlConfig, CacheControlProcessor, CacheWarmerConfig,
    CacheableRole,
};
pub use processors::{
    EssentialEvent, EssentialPreservationProcessor, FilterProcessor, HistoryProcessor,
    LastNProcessor, NoOpProcessor, ProcessorChain,
};
pub use summary::{
    ChangeType, CompactionSummary, Decision, ErrorRecord, FileChange, FileSummary,
    OrchestrationSummary, RuntimeTaskSummary, TaskComplexity, TaskOutcome, VerificationSummary,
    calculate_threshold, threshold_to_tokens,
};
pub use tool_output_pruner::{
    PRUNE_MINIMUM_TOKENS, PRUNE_PROTECT_TOKENS, TAG_KEEP_OUTPUT, TAG_PRUNE_OUTPUT,
    ToolOutputPruner, ToolOutputTag,
};
pub use turns::{UserTurn, find_turn_boundary, group_into_turns};
pub use visibility::{MessageMetadata, VISIBILITY_METADATA_KEY};

// Re-export key types from dedup
pub use dedup::{ContextTier, DuplicateNotice, DuplicateReadDetector, ReadId, TierStats};

// Re-export key types from thinning
pub use thinning::{
    DEFAULT_THRESHOLD_BYTES, MAX_FILE_AGE_SECS, OutputThinner, THINNED_FILE_PREFIX, ThinnedOutput,
    ThinnerConfig,
};

// Re-export key types from threshold
pub use threshold::{
    CONTEXT_WINDOW_PATTERNS, get_context_threshold, get_context_threshold_for_window,
    get_context_window, get_context_window_for_id, get_safe_buffer,
};

use crate::reliability::{
    BudgetedContext, ContextBudgetComposer, RetrievalAdapter, RetrievalContextInput,
};

/// Build runtime reliability context using retrieval-first ordering under a strict token budget.
///
/// This is the canonical runtime assembly path for reliability-sensitive context sections.
pub fn assemble_reliability_runtime_context(
    total_budget: usize,
    input: RetrievalContextInput,
) -> BudgetedContext {
    ContextBudgetComposer::compose_retrieval_first(total_budget, input)
}

pub fn assemble_reliability_runtime_context_with_adapter(
    total_budget: usize,
    input: RetrievalContextInput,
    adapter: Option<&dyn RetrievalAdapter>,
) -> BudgetedContext {
    ContextBudgetComposer::compose_retrieval_first_with_adapter(total_budget, input, adapter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn integration_input() -> RetrievalContextInput {
        RetrievalContextInput {
            critical_rules: "rule_a rule_b rule_c".to_string(),
            active_task_contract: "contract_a contract_b contract_c".to_string(),
            latest_evidence: "evidence_a evidence_b evidence_c".to_string(),
            compacted_state_digest: "digest_a digest_b digest_c".to_string(),
            code_retrieval: "code_a code_b code_c code_d code_e".to_string(),
        }
    }

    #[test]
    fn reliability_budget_retrieval_first_integration() {
        let out = assemble_reliability_runtime_context(18, integration_input());
        assert!(!out.included_sections.is_empty());
        assert_eq!(out.included_sections[0], "critical_rules");
        assert!(
            out.included_sections
                .iter()
                .any(|section| section == "active_task_contract"),
            "active task contract should survive within the runtime budget"
        );
    }

    #[test]
    fn reliability_budget_retrieval_first_preserves_critical_under_pressure() {
        let out = assemble_reliability_runtime_context(4, integration_input());
        assert!(out.total_used_tokens <= 4);
        assert!(
            out.included_sections
                .iter()
                .any(|section| section == "critical_rules"),
            "critical rules must survive tight budgets"
        );
        assert!(
            !out.truncated_sections.is_empty(),
            "tight budget should force truncation reporting"
        );
    }

    struct FailingAdapter;

    impl RetrievalAdapter for FailingAdapter {
        fn retrieve(
            &self,
            _local: &RetrievalContextInput,
        ) -> Result<RetrievalContextInput, String> {
            Err("external backend unavailable".to_string())
        }
    }

    #[test]
    fn reliability_retrieval_adapter_fallback() {
        let out = assemble_reliability_runtime_context_with_adapter(
            12,
            integration_input(),
            Some(&FailingAdapter),
        );
        assert!(
            out.rendered.contains("rule_a"),
            "fallback should retain local critical-rules content"
        );
        assert!(
            out.included_sections
                .iter()
                .any(|section| section == "critical_rules"),
            "critical rules section must remain after adapter fallback"
        );
    }
}
