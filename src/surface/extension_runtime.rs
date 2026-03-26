use crate::agent::{AgentSession, PreWarmedExtensionRuntime};
use crate::auth::AuthStorage;
use crate::cli;
use crate::config::Config;
use crate::error::Result;
use crate::extensions::{ExtensionManager, ExtensionRuntimeHandle, RepairPolicyMode};
use crate::extensions_js::PiJsRuntimeConfig;
use crate::models::{ModelRegistry, OAuthConfig};
use crate::resources::ResourceLoader;
use crate::surface::extension_policy::maybe_print_extension_policy_migration_notice;
use crate::tools::ToolRegistry;
use asupersync::runtime::{JoinHandle, RuntimeHandle};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceExtensionRuntimeFlavor {
    Js,
    NativeRust,
}

pub type SurfaceExtensionPrewarmHandle = (
    ExtensionManager,
    Arc<ToolRegistry>,
    JoinHandle<anyhow::Result<ExtensionRuntimeHandle>>,
);

fn parse_bool_flag_value(flag_name: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(crate::error::Error::validation(format!(
            "Invalid boolean value for extension flag --{flag_name}: \"{raw}\". Use one of: true,false,1,0,yes,no,on,off."
        ))),
    }
}

pub fn coerce_extension_flag_value(
    flag: &cli::ExtensionCliFlag,
    declared_type: &str,
) -> Result<Value> {
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
                )));
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
                )));
            };
            Ok(Value::Number(number))
        }
        _ => {
            let Some(raw) = flag.value.as_deref() else {
                return Err(crate::error::Error::validation(format!(
                    "Extension flag --{} requires a value.",
                    flag.name
                )));
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
            )));
        }

        for spec in matches {
            let Some(extension_id) = spec.get("extension_id").and_then(Value::as_str) else {
                return Err(crate::error::Error::validation(format!(
                    "Extension flag --{} cannot be set because extension metadata is missing extension_id.",
                    cli_flag.name
                )));
            };
            if extension_id.trim().is_empty() {
                return Err(crate::error::Error::validation(format!(
                    "Extension flag --{} cannot be set because extension_id is empty.",
                    cli_flag.name
                )));
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

pub fn resolve_surface_extension_runtime_flavor(
    resources: &ResourceLoader,
) -> Result<Option<SurfaceExtensionRuntimeFlavor>> {
    let mut has_js_extensions = false;
    let mut has_native_extensions = false;
    for entry in resources.extensions() {
        match crate::extensions::resolve_extension_load_spec(entry) {
            Ok(crate::extensions::ExtensionLoadSpec::NativeRust(_)) => has_native_extensions = true,
            Ok(crate::extensions::ExtensionLoadSpec::Js(_)) => has_js_extensions = true,
            #[cfg(feature = "wasm-host")]
            Ok(crate::extensions::ExtensionLoadSpec::Wasm(_)) => {}
            Err(err) => {
                return Err(err);
            }
        }
    }

    match (has_js_extensions, has_native_extensions) {
        (true, true) => Err(crate::error::Error::validation(
            "Mixed extension runtimes are not supported in one session yet. Use either JS/TS extensions (QuickJS) or native-rust descriptors (*.native.json), but not both at once."
                .to_string(),
        )),
        (true, false) => Ok(Some(SurfaceExtensionRuntimeFlavor::Js)),
        (false, true) => Ok(Some(SurfaceExtensionRuntimeFlavor::NativeRust)),
        (false, false) => Ok(None),
    }
}

pub fn spawn_surface_extension_prewarm(
    runtime_handle: &RuntimeHandle,
    cli: &cli::Cli,
    config: &Config,
    cwd: &Path,
    extension_runtime_flavor: Option<SurfaceExtensionRuntimeFlavor>,
) -> Option<SurfaceExtensionPrewarmHandle> {
    let prewarm_policy = config
        .resolve_extension_policy_with_metadata(cli.extension_policy.as_deref())
        .policy;
    let prewarm_repair = config.resolve_repair_policy_with_metadata(cli.repair_policy.as_deref());
    let prewarm_repair_mode = if prewarm_repair.source == "default" {
        RepairPolicyMode::AutoStrict
    } else {
        prewarm_repair.effective_mode
    };
    let prewarm_memory_limit_bytes =
        (prewarm_policy.max_memory_mb as usize).saturating_mul(1024 * 1024);

    if matches!(
        extension_runtime_flavor,
        None | Some(SurfaceExtensionRuntimeFlavor::Js)
    ) {
        let _ = extension_runtime_flavor?;

        let pre_enabled_tools = cli.enabled_tools();
        let pre_mgr = ExtensionManager::new();
        pre_mgr.set_cwd(cwd.display().to_string());

        let pre_tools = Arc::new(ToolRegistry::new(&pre_enabled_tools, cwd, Some(config)));

        let resolved_risk = config.resolve_extension_risk_with_metadata();
        pre_mgr.set_runtime_risk_config(resolved_risk.settings);

        let pre_mgr_for_runtime = pre_mgr.clone();
        let pre_tools_for_runtime = Arc::clone(&pre_tools);
        let prewarm_policy_for_runtime = prewarm_policy.clone();
        let prewarm_cwd = cwd.display().to_string();
        return Some((
            pre_mgr,
            pre_tools,
            runtime_handle.spawn(async move {
                let mut js_config = PiJsRuntimeConfig {
                    cwd: prewarm_cwd,
                    repair_mode: AgentSession::runtime_repair_mode_from_policy_mode(
                        prewarm_repair_mode,
                    ),
                    ..PiJsRuntimeConfig::default()
                };
                js_config.limits.memory_limit_bytes =
                    Some(prewarm_memory_limit_bytes).filter(|bytes| *bytes > 0);
                let runtime = crate::extensions::JsExtensionRuntimeHandle::start_with_policy(
                    js_config,
                    pre_tools_for_runtime,
                    pre_mgr_for_runtime,
                    prewarm_policy_for_runtime,
                )
                .await
                .map(ExtensionRuntimeHandle::Js)
                .map_err(anyhow::Error::new)?;
                tracing::info!(
                    event = "pi.extension_runtime.engine_decision",
                    stage = "surface_prewarm",
                    requested = "quickjs",
                    selected = "quickjs",
                    fallback = false,
                    "Extension runtime engine selected for prewarm (legacy JS/TS)"
                );
                Ok::<ExtensionRuntimeHandle, anyhow::Error>(runtime)
            }),
        ));
    }

    let pre_enabled_tools = cli.enabled_tools();
    let pre_mgr = ExtensionManager::new();
    pre_mgr.set_cwd(cwd.display().to_string());
    let pre_tools = Arc::new(ToolRegistry::new(&pre_enabled_tools, cwd, Some(config)));

    let resolved_risk = config.resolve_extension_risk_with_metadata();
    pre_mgr.set_runtime_risk_config(resolved_risk.settings);

    Some((
        pre_mgr,
        pre_tools,
        runtime_handle.spawn(async move {
            let runtime = crate::extensions::NativeRustExtensionRuntimeHandle::start()
                .await
                .map(ExtensionRuntimeHandle::NativeRust)
                .map_err(anyhow::Error::new)?;
            tracing::info!(
                event = "pi.extension_runtime.engine_decision",
                stage = "surface_prewarm",
                requested = "native-rust",
                selected = "native-rust",
                fallback = false,
                "Extension runtime engine selected for prewarm (native-rust)"
            );
            Ok::<ExtensionRuntimeHandle, anyhow::Error>(runtime)
        }),
    ))
}

#[allow(clippy::too_many_arguments)]
pub async fn activate_extensions_for_session(
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
    extension_prewarm_handle: Option<SurfaceExtensionPrewarmHandle>,
) -> Result<()> {
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
        )));
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
            ));
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
