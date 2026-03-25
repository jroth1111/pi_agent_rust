//! CLI-facing extension and repair policy diagnostics.
//!
//! This lives in the surface layer because it formats operator-facing
//! diagnostics and remediation guidance without owning runtime authority.

use anyhow::Result;
use pi::extensions::{ALL_CAPABILITIES, Capability, PolicyDecision};

fn policy_config_example(profile: &str, allow_dangerous: bool) -> serde_json::Value {
    serde_json::json!({
        "extensionPolicy": {
            "profile": profile,
            "allowDangerous": allow_dangerous,
        }
    })
}

pub fn extension_policy_migration_guardrails(
    resolved: &pi::config::ResolvedExtensionPolicy,
) -> serde_json::Value {
    serde_json::json!({
        "default_profile": "safe",
        "active_default_profile": resolved.profile_source == "default" && resolved.effective_profile == "safe",
        "profile_source": resolved.profile_source,
        "safe_by_default_reason": "Fresh installs deny dangerous capabilities unless explicitly opted in.",
        "opt_in_cli": {
            "balanced_prompt_mode": "pi --extension-policy balanced <your command>",
            "balanced_with_dangerous_caps": "PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced <your command>",
            "temporary_permissive": "pi --extension-policy permissive <your command>",
        },
        "settings_examples": {
            "safe_default": policy_config_example("safe", false),
            "balanced_prompt_mode": policy_config_example("balanced", false),
            "balanced_with_dangerous_caps": policy_config_example("balanced", true),
            "temporary_permissive": policy_config_example("permissive", false),
        },
        "revert_to_safe_cli": "pi --extension-policy safe <your command>",
    })
}

pub fn maybe_print_extension_policy_migration_notice(
    resolved: &pi::config::ResolvedExtensionPolicy,
) {
    if resolved.profile_source == "default" && resolved.effective_profile == "safe" {
        eprintln!(
            "Note: extension policy now defaults to `safe` (dangerous capabilities denied by default)."
        );
        eprintln!(
            "If an extension needs broader access, try `--extension-policy balanced` and optionally `PI_EXTENSION_ALLOW_DANGEROUS=1`."
        );
    }
}

fn policy_reason_detail(reason: &str) -> &'static str {
    match reason {
        "extension_deny" => "Denied by an extension-specific override.",
        "deny_caps" => "Denied by the global deny list.",
        "extension_allow" => "Allowed by an extension-specific override.",
        "default_caps" => "Allowed by profile defaults.",
        "not_in_default_caps" => "Not part of profile defaults in strict mode.",
        "prompt_required" => "Requires an explicit runtime prompt decision.",
        "permissive" => "Allowed because permissive mode bypasses prompts.",
        "empty_capability" => "Invalid request: capability name is empty.",
        _ => "Policy engine returned an implementation-defined reason.",
    }
}

fn capability_remediation(capability: Capability, decision: PolicyDecision) -> serde_json::Value {
    let is_dangerous = capability.is_dangerous();

    let (to_allow_cli, to_allow_config, recommendation) = match (is_dangerous, decision) {
        (true, PolicyDecision::Deny) => (
            vec![
                "PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced <your command>",
                "pi --extension-policy permissive <your command>",
            ],
            vec![
                policy_config_example("balanced", true),
                policy_config_example("permissive", false),
            ],
            "Prefer balanced + allowDangerous=true over permissive for narrower blast radius.",
        ),
        (true, PolicyDecision::Prompt) => (
            vec![
                "Approve the runtime capability prompt (Allow once/always).",
                "pi --extension-policy permissive <your command>",
            ],
            vec![
                policy_config_example("balanced", true),
                policy_config_example("permissive", false),
            ],
            "Use prompt approvals first; move to permissive only if prompts are operationally impossible.",
        ),
        (true, PolicyDecision::Allow) => (
            Vec::new(),
            Vec::new(),
            "Capability is already allowed; keep this only if the extension truly needs it.",
        ),
        (false, PolicyDecision::Deny) => (
            vec![
                "pi --extension-policy balanced <your command>",
                "pi --extension-policy permissive <your command>",
            ],
            vec![
                policy_config_example("balanced", false),
                policy_config_example("permissive", false),
            ],
            "Balanced is usually enough; permissive should be temporary.",
        ),
        (false, PolicyDecision::Prompt) => (
            vec![
                "Approve the runtime capability prompt (Allow once/always).",
                "pi --extension-policy permissive <your command>",
            ],
            vec![
                policy_config_example("balanced", false),
                policy_config_example("permissive", false),
            ],
            "Prompt mode keeps explicit approval in the loop while preserving least privilege.",
        ),
        (false, PolicyDecision::Allow) => (
            Vec::new(),
            Vec::new(),
            "Capability is already allowed in the active profile.",
        ),
    };

    let to_restrict_cli = if is_dangerous {
        vec![
            "pi --extension-policy balanced <your command>",
            "pi --extension-policy safe <your command>",
        ]
    } else {
        vec!["pi --extension-policy safe <your command>"]
    };
    let to_restrict_config = if is_dangerous {
        vec![
            policy_config_example("balanced", false),
            policy_config_example("safe", false),
        ]
    } else {
        vec![policy_config_example("safe", false)]
    };

    serde_json::json!({
        "dangerous_capability": is_dangerous,
        "to_allow_cli": to_allow_cli,
        "to_allow_config_examples": to_allow_config,
        "to_restrict_cli": to_restrict_cli,
        "to_restrict_config_examples": to_restrict_config,
        "recommendation": recommendation,
    })
}

pub fn print_resolved_extension_policy(
    resolved: &pi::config::ResolvedExtensionPolicy,
) -> Result<()> {
    let capability_decisions = ALL_CAPABILITIES
        .iter()
        .map(|capability| {
            let check = resolved.policy.evaluate(capability.as_str());
            serde_json::json!({
                "capability": capability.as_str(),
                "decision": check.decision,
                "reason": check.reason,
                "reason_detail": policy_reason_detail(&check.reason),
                "remediation": capability_remediation(*capability, check.decision),
            })
        })
        .collect::<Vec<_>>();

    let dangerous_capabilities = Capability::dangerous_list()
        .iter()
        .map(|capability| {
            let check = resolved.policy.evaluate(capability.as_str());
            serde_json::json!({
                "capability": capability.as_str(),
                "decision": check.decision,
                "reason": check.reason,
                "reason_detail": policy_reason_detail(&check.reason),
                "remediation": capability_remediation(*capability, check.decision),
            })
        })
        .collect::<Vec<_>>();

    let profile_presets = serde_json::json!([
        {
            "profile": "safe",
            "summary": "Strict deny-by-default profile.",
            "cli": "pi --extension-policy safe <your command>",
            "config_example": policy_config_example("safe", false),
        },
        {
            "profile": "balanced",
            "summary": "Prompt-based profile (legacy alias: standard).",
            "cli": "pi --extension-policy balanced <your command>",
            "config_example": policy_config_example("balanced", false),
        },
        {
            "profile": "permissive",
            "summary": "Allow-most profile for temporary troubleshooting.",
            "cli": "pi --extension-policy permissive <your command>",
            "config_example": policy_config_example("permissive", false),
        },
    ]);

    let payload = serde_json::json!({
        "requested_profile": resolved.requested_profile,
        "effective_profile": resolved.effective_profile,
        "profile_aliases": {
            "standard": "balanced",
        },
        "profile_source": resolved.profile_source,
        "allow_dangerous": resolved.allow_dangerous,
        "profile_presets": profile_presets,
        "dangerous_capability_opt_in": {
            "cli": "PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced <your command>",
            "env_var": "PI_EXTENSION_ALLOW_DANGEROUS=1",
            "config_example": policy_config_example("balanced", true),
        },
        "migration_guardrails": extension_policy_migration_guardrails(resolved),
        "mode": resolved.policy.mode,
        "default_caps": resolved.policy.default_caps.clone(),
        "deny_caps": resolved.policy.deny_caps.clone(),
        "dangerous_capabilities": dangerous_capabilities,
        "capability_decisions": capability_decisions,
    });

    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

pub fn print_resolved_repair_policy(resolved: &pi::config::ResolvedRepairPolicy) -> Result<()> {
    let payload = serde_json::json!({
        "requested_mode": resolved.requested_mode,
        "effective_mode": resolved.effective_mode,
        "source": resolved.source,
        "modes": {
            "off": "Disable all repair functionality.",
            "suggest": "Only suggest fixes in diagnostics (default).",
            "auto-safe": "Automatically apply safe fixes (e.g., config updates).",
            "auto-strict": "Automatically apply all fixes including code changes.",
        },
        "cli_override": "pi --repair-policy <mode> <your command>",
        "env_var": "PI_REPAIR_POLICY=<mode>",
    });

    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}
