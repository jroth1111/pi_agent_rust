use crate::agent::{AgentSession, PreWarmedExtensionRuntime};
use crate::auth::AuthStorage;
use crate::cli;
use crate::config::Config;
use crate::error::Result;
use crate::extensions::{ExtensionManager, ExtensionRuntimeHandle, RepairPolicyMode};
use crate::models::{ModelRegistry, OAuthConfig};
use crate::surface::extension_policy::maybe_print_extension_policy_migration_notice;
use crate::tools::ToolRegistry;
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn parse_bool_flag_value(flag_name: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(crate::error::Error::validation(format!(
            "Invalid boolean value for extension flag --{flag_name}: \"{raw}\". Use one of: true,false,1,0,yes,no,on,off."
        ))
        .into()),
    }
}

fn coerce_extension_flag_value(flag: &cli::ExtensionCliFlag, declared_type: &str) -> Result<Value> {
    match declared_type.trim().to_ascii_lowercase().as_str() {
        "bool" | "boolean" => {
            if let Some(raw) = flag.value.as_deref() {
                Ok(Value::Bool(parse_bool_flag_value(&flag.name, raw)?))
            } else {
                Ok(Value::Bool(true))
            }
        }
        "number" | "int" | "integer" | "float" => {
            let Some(raw) = flag.value.as_deref() else {
                return Err(crate::error::Error::validation(format!(
                    "Extension flag --{} requires a numeric value.",
                    flag.name
                ))
                .into());
            };
            if let Ok(parsed) = raw.parse::<i64>() {
                return Ok(Value::Number(parsed.into()));
            }
            let parsed = raw.parse::<f64>().map_err(|_| {
                crate::error::Error::validation(format!(
                    "Invalid numeric value for extension flag --{}: \"{}\"",
                    flag.name, raw
                ))
            })?;
            let Some(number) = serde_json::Number::from_f64(parsed) else {
                return Err(crate::error::Error::validation(format!(
                    "Numeric value for extension flag --{} is not finite: \"{}\"",
                    flag.name, raw
                ))
                .into());
            };
            Ok(Value::Number(number))
        }
        _ => {
            let Some(raw) = flag.value.as_deref() else {
                return Err(crate::error::Error::validation(format!(
                    "Extension flag --{} requires a value.",
                    flag.name
                ))
                .into());
            };
            Ok(Value::String(raw.to_string()))
        }
    }
}

async fn apply_extension_cli_flags(
    manager: &ExtensionManager,
    extension_flags: &[cli::ExtensionCliFlag],
) -> Result<()> {
    if extension_flags.is_empty() {
        return Ok(());
    }

    let registered = manager.list_flags();
    let known_names: std::collections::BTreeSet<String> = registered
        .iter()
        .filter_map(|flag| flag.get("name").and_then(Value::as_str))
        .map(ToString::to_string)
        .collect();

    for cli_flag in extension_flags {
        let matches = registered
            .iter()
            .filter(|flag| {
                flag.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case(&cli_flag.name))
            })
            .collect::<Vec<_>>();

        if matches.is_empty() {
            let known = if known_names.is_empty() {
                "(none)".to_string()
            } else {
                known_names
                    .iter()
                    .map(|name| format!("--{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(crate::error::Error::validation(format!(
                "Unknown extension flag --{}. Registered extension flags: {known}",
                cli_flag.name
            ))
            .into());
        }

        for spec in matches {
            let Some(extension_id) = spec.get("extension_id").and_then(Value::as_str) else {
                return Err(crate::error::Error::validation(format!(
                    "Extension flag --{} cannot be set because extension metadata is missing extension_id.",
                    cli_flag.name
                ))
                .into());
            };
            if extension_id.trim().is_empty() {
                return Err(crate::error::Error::validation(format!(
                    "Extension flag --{} cannot be set because extension_id is empty.",
                    cli_flag.name
                ))
                .into());
            }
            let registered_name = spec.get("name").and_then(Value::as_str).ok_or_else(|| {
                crate::error::Error::validation(format!(
                    "Extension flag --{} is missing name metadata.",
                    cli_flag.name
                ))
            })?;
            let flag_type = spec.get("type").and_then(Value::as_str).unwrap_or("string");
            let value = coerce_extension_flag_value(cli_flag, flag_type)?;
            manager
                .set_flag_value(extension_id, registered_name, value)
                .await?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn activate_extensions_for_session<J>(
    agent_session: &mut AgentSession,
    enabled_tools: &[&str],
    cwd: &Path,
    config: &Config,
    extension_entries: &[PathBuf],
    extension_flags: &[cli::ExtensionCliFlag],
    auth: &mut AuthStorage,
    model_registry: &mut ModelRegistry,
    extension_policy_override: Option<&str>,
    repair_policy_override: Option<&str>,
    extension_prewarm_handle: Option<(ExtensionManager, Arc<ToolRegistry>, J)>,
) -> Result<()>
where
    J: Future<Output = anyhow::Result<ExtensionRuntimeHandle>>,
{
    if extension_entries.is_empty() {
        if extension_flags.is_empty() {
            return Ok(());
        }

        let rendered = extension_flags
            .iter()
            .map(cli::ExtensionCliFlag::display_name)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(crate::error::Error::validation(format!(
            "Extension flags were provided ({rendered}), but no extensions are loaded. Add extensions via --extension or remove the flags."
        ))
        .into());
    }

    let pre_warmed = if let Some((manager, tools, join_handle)) = extension_prewarm_handle {
        match join_handle.await {
            Ok(runtime) => {
                tracing::info!(
                    event = "pi.extension_runtime.prewarm.success",
                    runtime = runtime.runtime_name(),
                    "Pre-warmed extension runtime ready"
                );
                Some(PreWarmedExtensionRuntime {
                    manager,
                    runtime,
                    tools,
                })
            }
            Err(error) => {
                tracing::warn!(
                    event = "pi.extension_runtime.prewarm.failed",
                    error = %error,
                    "Extension runtime pre-warm failed, falling back to inline creation"
                );
                None
            }
        }
    } else {
        None
    };

    let resolved_ext_policy =
        config.resolve_extension_policy_with_metadata(extension_policy_override);
    let resolved_repair_policy = config.resolve_repair_policy_with_metadata(repair_policy_override);
    let effective_repair_policy = if resolved_repair_policy.source == "default" {
        RepairPolicyMode::AutoStrict
    } else {
        resolved_repair_policy.effective_mode
    };
    tracing::info!(
        event = "pi.extension_repair_policy.resolved",
        requested = %resolved_repair_policy.requested_mode,
        source = resolved_repair_policy.source,
        effective = ?effective_repair_policy,
        "Resolved extension repair policy for runtime"
    );
    maybe_print_extension_policy_migration_notice(&resolved_ext_policy);

    agent_session
        .enable_extensions_with_policy(
            enabled_tools,
            cwd,
            Some(config),
            extension_entries,
            Some(resolved_ext_policy.policy),
            Some(effective_repair_policy),
            pre_warmed,
        )
        .await?;

    if !extension_flags.is_empty() {
        if let Some(region) = &agent_session.extensions {
            apply_extension_cli_flags(region.manager(), extension_flags).await?;
        } else {
            return Err(crate::error::Error::validation(
                "Extension flags were provided, but extensions are not active in this session.",
            )
            .into());
        }
    }

    if let Some(region) = &agent_session.extensions {
        let ext_entries = region.manager().extension_model_entries();
        if !ext_entries.is_empty() {
            let ext_oauth_configs: std::collections::HashMap<String, OAuthConfig> = ext_entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .oauth_config
                        .as_ref()
                        .map(|cfg| (entry.model.provider.clone(), cfg.clone()))
                })
                .collect();

            model_registry.merge_entries(ext_entries);

            if !ext_oauth_configs.is_empty() {
                let client = crate::http::client::Client::new();
                if let Err(error) = auth
                    .refresh_expired_extension_oauth_tokens(&client, &ext_oauth_configs)
                    .await
                {
                    tracing::warn!(
                        event = "pi.auth.extension_oauth_refresh.failed",
                        error = %error,
                        "Failed to refresh extension OAuth tokens, continuing with existing credentials"
                    );
                }
            }
        }
    }

    Ok(())
}
