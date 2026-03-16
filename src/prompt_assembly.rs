use crate::model::Message;
use crate::prompt_plan::{
    PromptAssemblyPlan, PromptSectionKind, estimate_messages_tokens, estimate_text_tokens,
    estimate_tools_tokens,
};
use crate::provider::{Context, ToolDef};
use std::borrow::Cow;

#[derive(Debug, Clone, Default)]
pub struct PromptAssemblyInputs {
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolDef>,
    pub repo_policy: Option<String>,
    pub task_manifest: Option<String>,
    pub retrieval_bundle: Option<String>,
    pub evidence: Option<String>,
    pub fallback_history: Vec<Message>,
    pub fresh_turn: Vec<Message>,
}

impl PromptAssemblyInputs {
    pub fn from_history(
        system_prompt: Option<String>,
        tools: Vec<ToolDef>,
        history: Vec<Message>,
    ) -> Self {
        Self {
            system_prompt,
            tools,
            fallback_history: history,
            ..Self::default()
        }
    }
}

pub fn assemble_prompt_plan(inputs: PromptAssemblyInputs) -> PromptAssemblyPlan {
    let mut plan = PromptAssemblyPlan {
        system_prompt: inputs.system_prompt,
        tools: inputs.tools,
        ..PromptAssemblyPlan::default()
    };

    if let Some(system_prompt) = &plan.system_prompt {
        plan.add_section(
            PromptSectionKind::StaticPrefix,
            estimate_text_tokens(system_prompt),
        );
    }

    let tool_tokens = estimate_tools_tokens(&plan.tools);
    if tool_tokens > 0 {
        plan.add_section(PromptSectionKind::StaticPrefix, tool_tokens);
    }

    if let Some(repo_policy) = &inputs.repo_policy {
        let tokens = estimate_text_tokens(repo_policy);
        if tokens > 0 {
            plan.add_section(PromptSectionKind::RepoPolicy, tokens);
        }
    }

    if let Some(task_manifest) = &inputs.task_manifest {
        let tokens = estimate_text_tokens(task_manifest);
        if tokens > 0 {
            plan.add_section(PromptSectionKind::TaskManifest, tokens);
        }
    }

    if let Some(retrieval_bundle) = &inputs.retrieval_bundle {
        let tokens = estimate_text_tokens(retrieval_bundle);
        if tokens > 0 {
            plan.add_section(PromptSectionKind::RetrievalBundle, tokens);
        }
    }

    if let Some(evidence) = &inputs.evidence {
        let tokens = estimate_text_tokens(evidence);
        if tokens > 0 {
            plan.add_section(PromptSectionKind::Evidence, tokens);
        }
    }

    let fallback_tokens = estimate_messages_tokens(&inputs.fallback_history);
    if fallback_tokens > 0 {
        plan.add_section(PromptSectionKind::FallbackHistory, fallback_tokens);
    }

    let fresh_turn_tokens = estimate_messages_tokens(&inputs.fresh_turn);
    if fresh_turn_tokens > 0 {
        plan.add_section(PromptSectionKind::FreshTurn, fresh_turn_tokens);
    }

    plan.messages = inputs.fallback_history;
    plan.messages.extend(inputs.fresh_turn);
    plan
}

pub fn context_from_prompt_plan(plan: &PromptAssemblyPlan) -> Context<'static> {
    Context::owned(
        plan.system_prompt.clone(),
        plan.messages.clone(),
        plan.tools.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, UserContent, UserMessage};
    use serde_json::json;

    #[test]
    fn assemble_prompt_plan_tracks_named_sections() {
        let plan = assemble_prompt_plan(PromptAssemblyInputs {
            system_prompt: Some("system".to_string()),
            tools: vec![ToolDef {
                name: "read".to_string(),
                description: "Read file contents".to_string(),
                parameters: json!({"type": "object"}),
            }],
            repo_policy: Some("keep changes scoped".to_string()),
            task_manifest: Some("task: tighten prompt flow".to_string()),
            retrieval_bundle: Some("symbol: Agent::build_context".to_string()),
            evidence: Some("no evidence yet".to_string()),
            fallback_history: vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            fresh_turn: vec![Message::User(UserMessage {
                content: UserContent::Text("follow-up".to_string()),
                timestamp: 1,
            })],
        });

        let kinds: Vec<_> = plan.sections.iter().map(|section| section.kind).collect();
        assert!(kinds.contains(&PromptSectionKind::StaticPrefix));
        assert!(kinds.contains(&PromptSectionKind::RepoPolicy));
        assert!(kinds.contains(&PromptSectionKind::TaskManifest));
        assert!(kinds.contains(&PromptSectionKind::RetrievalBundle));
        assert!(kinds.contains(&PromptSectionKind::Evidence));
        assert!(kinds.contains(&PromptSectionKind::FallbackHistory));
        assert!(kinds.contains(&PromptSectionKind::FreshTurn));
        assert_eq!(plan.messages.len(), 2);
        assert!(plan.token_breakdown.total_estimate() > 0);
    }

    #[test]
    fn context_from_prompt_plan_uses_owned_payloads() {
        let plan = assemble_prompt_plan(PromptAssemblyInputs::from_history(
            Some("system".to_string()),
            vec![ToolDef {
                name: "read".to_string(),
                description: "Read".to_string(),
                parameters: json!({"type": "object"}),
            }],
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
        ));

        let context = context_from_prompt_plan(&plan);
        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.tools.len(), 1);
        assert_eq!(context.system_prompt.as_deref(), Some("system"));
    }
}
