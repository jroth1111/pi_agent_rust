//! Pi - High-performance AI coding agent CLI
//!
//! Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance: Sub-100ms startup, smooth TUI at 60fps
//! - Reliability: No panics in normal operation
//! - Efficiency: Single binary, minimal dependencies

#![forbid(unsafe_code)]
// Allow dead code and unused async during scaffolding phase - remove once implementation is complete
#![allow(
    dead_code,
    clippy::unused_async,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::uninlined_format_args,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value,
    clippy::needless_pass_by_ref_mut,
    clippy::collapsible_match,
    clippy::default_trait_access,
    clippy::field_reassign_with_default,
    clippy::items_after_statements,
    clippy::manual_let_else,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::unused_self
)]

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::{Result, bail};
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
use asupersync::sync::Mutex;
use bubbletea::{Cmd, KeyMsg, KeyType, Message as BubbleMessage, Program, quit};
use clap::error::ErrorKind;
use pi::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession};
use pi::app::StartupError;
use pi::auth::AuthStorage;
use pi::cli;
use pi::compaction::ResolvedCompactionSettings;
use pi::config::Config;
use pi::config::SettingsScope;
use pi::extension_index::ExtensionIndexStore;
use pi::extensions::{
    ExtensionLoadSpec, ExtensionRuntimeHandle, JsExtensionRuntimeHandle,
    NativeRustExtensionRuntimeHandle, resolve_extension_load_spec,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::model::{AssistantMessage, ContentBlock, StopReason};
use pi::models::{ModelEntry, ModelRegistry, default_models_path};
use pi::package_manager::{
    PackageEntry, PackageManager, PackageScope, ResolvedPaths, ResolvedResource, ResourceOrigin,
};
use pi::provider::InputType;
use pi::provider_metadata::PROVIDER_METADATA;
use pi::providers;
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::session_index::SessionIndex;
#[cfg(test)]
use pi::surface::auth_setup::{SetupCredentialKind, provider_choice_from_token};
use pi::surface::extension_policy::{
    print_resolved_extension_policy, print_resolved_repair_policy,
};
use pi::surface::extension_runtime::activate_extensions_for_session;
use pi::surface::startup::{
    NonInteractiveStartupRequirement, SurfaceStartupLoopAction, load_model_registry_with_warning,
    recover_surface_startup_error_for_selection,
};
use pi::tools::ToolRegistry;
use pi::tui::PiConsole;
use serde::Serialize;
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

const EXIT_CODE_FAILURE: i32 = 1;
const EXIT_CODE_USAGE: i32 = 2;
const USAGE_ERROR_PATTERNS: &[&str] = &[
    "@file arguments are not supported in rpc mode",
    "--api-key requires a model to be specified via --provider/--model or --models",
    "unknown --only categories",
    "--only must include at least one category",
    "theme file not found",
    "theme spec is empty",
];

fn main() {
    if let Err(err) = main_impl() {
        let exit_code = exit_code_for_error(&err);
        print_error_with_hints(&err);
        std::process::exit(exit_code);
    }
}

fn parse_cli_args(raw_args: Vec<String>) -> Result<Option<(cli::Cli, Vec<cli::ExtensionCliFlag>)>> {
    match cli::parse_with_extension_flags(raw_args) {
        Ok(parsed) => Ok(Some((parsed.cli, parsed.extension_flags))),
        Err(err) => {
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                err.print()?;
                return Ok(None);
            }
            Err(anyhow::Error::new(err))
        }
    }
}

fn parse_cli_from_env() -> Result<Option<(cli::Cli, Vec<cli::ExtensionCliFlag>)>> {
    parse_cli_args(std::env::args().collect())
}

#[allow(clippy::too_many_lines)]
fn main_impl() -> Result<()> {
    // Parse CLI arguments
    let Some((cli, extension_flags)) = parse_cli_from_env()? else {
        return Ok(());
    };

    // Validate theme file paths.
    // Named themes (without .json, /, ~) are validated later after resource loading.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    validate_theme_path_spec(cli.theme.as_deref(), &cwd)?;

    if cli.version {
        print_version();
        return Ok(());
    }

    // Ultra-fast paths that don't need tracing or the async runtime.
    if let Some(command) = &cli.command {
        match command {
            cli::Commands::List => {
                let manager = PackageManager::new(cwd);
                handle_package_list_blocking(&manager)?;
                return Ok(());
            }
            cli::Commands::Info { name } => {
                handle_info_blocking(name)?;
                return Ok(());
            }
            cli::Commands::Search {
                query,
                tag,
                sort,
                limit,
            } => {
                if handle_search_blocking(query, tag.as_deref(), sort, *limit)? {
                    return Ok(());
                }
            }
            cli::Commands::Doctor {
                path,
                format,
                policy,
                fix,
                only,
            } => {
                handle_doctor(
                    &cwd,
                    path.as_deref(),
                    format,
                    policy.as_deref(),
                    *fix,
                    only.as_deref(),
                )?;
                return Ok(());
            }
            _ => {}
        }
    }

    if cli.explain_extension_policy {
        let config = Config::load()?;
        let resolved =
            config.resolve_extension_policy_with_metadata(cli.extension_policy.as_deref());
        print_resolved_extension_policy(&resolved)?;
        return Ok(());
    }

    if cli.explain_repair_policy {
        let config = Config::load()?;
        let resolved = config.resolve_repair_policy_with_metadata(cli.repair_policy.as_deref());
        print_resolved_repair_policy(&resolved)?;
        return Ok(());
    }

    // List-providers is a fast offline query that uses only static metadata.
    if cli.list_providers {
        list_providers();
        return Ok(());
    }

    // List-models is an offline query; avoid loading resources or booting the runtime when possible.
    //
    // IMPORTANT: if extension compat scanning is enabled, or explicit CLI extensions are provided,
    // we must boot the normal startup path so the compat ledger can be emitted deterministically.
    if cli.command.is_none() {
        if let Some(pattern) = &cli.list_models {
            let compat_scan_enabled =
                std::env::var("PI_EXT_COMPAT_SCAN")
                    .ok()
                    .is_some_and(|value| {
                        matches!(
                            value.trim().to_ascii_lowercase().as_str(),
                            "1" | "true" | "yes" | "on"
                        )
                    });
            let has_cli_extensions = !cli.extension.is_empty();

            if !compat_scan_enabled && !has_cli_extensions {
                // Note: we intentionally skip OAuth refresh here to keep this path fast and offline.
                let auth = AuthStorage::load(Config::auth_path())?;
                let models_path = default_models_path(&Config::global_dir());
                let registry = ModelRegistry::load(&auth, Some(models_path));
                if let Some(error) = registry.error() {
                    eprintln!("Warning: models.json error: {error}");
                }
                list_models(&registry, pattern.as_deref());
                return Ok(());
            }
        }
    }

    // Initialize logging (skip for ultra-fast paths like --version)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .with_writer(io::stderr)
        .init();

    // Run the application
    let reactor = create_reactor()?;
    let runtime = RuntimeBuilder::multi_thread()
        .blocking_threads(1, 8)
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let handle = runtime.handle();
    let runtime_handle = handle.clone();
    let join = handle.spawn(Box::pin(run(cli, extension_flags, runtime_handle)));
    runtime.block_on(join)
}

fn print_error_with_hints(err: &anyhow::Error) {
    for cause in err.chain() {
        if let Some(pi_error) = cause.downcast_ref::<pi::error::Error>() {
            eprint!("{}", pi::error_hints::format_error_with_hints(pi_error));
            return;
        }
    }

    eprintln!("{err}");
}

fn exit_code_for_error(err: &anyhow::Error) -> i32 {
    if is_usage_error(err) {
        EXIT_CODE_USAGE
    } else {
        EXIT_CODE_FAILURE
    }
}

fn is_usage_error(err: &anyhow::Error) -> bool {
    if err
        .chain()
        .any(|cause| cause.downcast_ref::<clap::Error>().is_some())
    {
        return true;
    }

    if err.chain().any(|cause| {
        cause
            .downcast_ref::<pi::error::Error>()
            .is_some_and(|pi_error| matches!(pi_error, pi::error::Error::Validation(_)))
    }) {
        return true;
    }

    let message = err.to_string().to_ascii_lowercase();
    USAGE_ERROR_PATTERNS
        .iter()
        .any(|pattern| message.contains(pattern))
}

fn validate_theme_path_spec(theme_spec: Option<&str>, cwd: &Path) -> Result<()> {
    if let Some(theme_spec) = theme_spec {
        if pi::theme::looks_like_theme_path(theme_spec) {
            pi::theme::Theme::resolve_spec(theme_spec, cwd).map_err(anyhow::Error::new)?;
        }
    }
    Ok(())
}

fn apply_cli_retry_override(config: &mut Config, cli: &cli::Cli) {
    if !cli.retry {
        return;
    }

    let mut retry = config.retry.clone().unwrap_or_default();
    retry.enabled = Some(true);
    config.retry = Some(retry);
}

struct StartupResources {
    config: Config,
    resources: ResourceLoader,
    auth: AuthStorage,
    resource_cli: ResourceCliOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtensionRuntimeFlavor {
    Js,
    NativeRust,
}

struct PreparedSurfaceInputs {
    global_dir: PathBuf,
    models_path: PathBuf,
    model_registry: ModelRegistry,
    initial: Option<pi::app::InitialMessage>,
    messages: Vec<String>,
    surface_bootstrap: pi::surface::CliSurfaceBootstrap,
    mode: String,
    non_interactive_guard: Option<pi::surface::NonInteractiveGuard>,
    scoped_patterns: Vec<String>,
}

async fn load_startup_resources(
    cli: &cli::Cli,
    extension_flags: &[cli::ExtensionCliFlag],
    cwd: &Path,
) -> Result<StartupResources> {
    let mut config = Config::load()?;
    if let Some(theme_spec) = cli.theme.as_deref() {
        // Theme already validated above.
        config.theme = Some(theme_spec.to_string());
    }
    apply_cli_retry_override(&mut config, cli);
    spawn_session_index_maintenance();

    let package_manager = PackageManager::new(cwd.to_path_buf());
    let resource_cli = ResourceCliOptions {
        no_skills: cli.no_skills,
        no_prompt_templates: cli.no_prompt_templates,
        no_extensions: cli.no_extensions,
        no_themes: cli.no_themes,
        skill_paths: cli.skill.clone(),
        prompt_paths: cli.prompt_template.clone(),
        extension_paths: cli.extension.clone(),
        theme_paths: cli.theme_path.clone(),
    };

    let auth_path = Config::auth_path();
    let (resources_result, auth_result) = futures::future::join(
        ResourceLoader::load(&package_manager, cwd, &config, &resource_cli),
        AuthStorage::load_async(auth_path),
    )
    .await;

    let resources = match resources_result {
        Ok(resources) => resources,
        Err(err) => {
            eprintln!("Warning: Failed to load skills/prompts: {err}");
            ResourceLoader::empty(config.enable_skill_commands())
        }
    };

    if !extension_flags.is_empty() && resources.extensions().is_empty() {
        let rendered = extension_flags
            .iter()
            .map(cli::ExtensionCliFlag::display_name)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(pi::error::Error::validation(format!(
            "Extension flags were provided ({rendered}), but no extensions are loaded. \
             Add extensions via --extension or remove the flags."
        ))
        .into());
    }

    Ok(StartupResources {
        config,
        resources,
        auth: auth_result?,
        resource_cli,
    })
}

fn resolve_extension_runtime_flavor(
    resources: &ResourceLoader,
) -> Result<Option<ExtensionRuntimeFlavor>> {
    let mut has_js_extensions = false;
    let mut has_native_extensions = false;
    for entry in resources.extensions() {
        match resolve_extension_load_spec(entry) {
            Ok(ExtensionLoadSpec::NativeRust(_)) => has_native_extensions = true,
            Ok(ExtensionLoadSpec::Js(_)) => has_js_extensions = true,
            #[cfg(feature = "wasm-host")]
            Ok(ExtensionLoadSpec::Wasm(_)) => {}
            Err(err) => {
                return Err(anyhow::Error::new(err));
            }
        }
    }

    match (has_js_extensions, has_native_extensions) {
        (true, true) => Err(pi::error::Error::validation(
            "Mixed extension runtimes are not supported in one session yet. Use either JS/TS extensions (QuickJS) or native-rust descriptors (*.native.json), but not both at once."
                .to_string(),
        )
        .into()),
        (true, false) => Ok(Some(ExtensionRuntimeFlavor::Js)),
        (false, true) => Ok(Some(ExtensionRuntimeFlavor::NativeRust)),
        (false, false) => Ok(None),
    }
}

async fn refresh_startup_auth(auth: &mut AuthStorage) -> Result<()> {
    auth.refresh_expired_oauth_tokens().await?;

    // Prune stale credentials that are well past expiry and lack refresh metadata.
    // 7-day cutoff (in milliseconds).
    let pruned = auth.prune_stale_credentials(7 * 24 * 60 * 60 * 1000);
    if !pruned.is_empty() {
        tracing::info!(
            pruned_providers = ?pruned,
            "Pruned stale credentials during startup"
        );
        auth.save()?;
    }

    Ok(())
}

async fn prepare_surface_inputs(
    cli: &mut cli::Cli,
    config: &Config,
    auth: &AuthStorage,
    cwd: &Path,
) -> Result<Option<PreparedSurfaceInputs>> {
    let global_dir = Config::global_dir();
    let models_path = default_models_path(&global_dir);
    let model_registry = load_model_registry_with_warning(auth, &models_path);
    if let Some(pattern) = &cli.list_models {
        list_models(&model_registry, pattern.as_deref());
        return Ok(None);
    }

    if cli.mode.as_deref() != Some("rpc") {
        let stdin_content = read_piped_stdin()?;
        pi::app::apply_piped_stdin(cli, stdin_content);
    }

    // Auto-detect print mode: if the user passed positional message args (e.g. `pi "hello"`)
    // or stdin was piped, run in non-interactive print mode automatically.
    if !cli.print && cli.mode.is_none() && !cli.message_args().is_empty() {
        cli.print = true;
    }

    pi::app::normalize_cli(cli);

    if let Some(export_path) = cli.export.clone() {
        let output = cli.message_args().first().map(ToString::to_string);
        let output_path = pi::surface::export_session_html(&export_path, output.as_deref()).await?;
        println!("Exported to: {}", output_path.display());
        return Ok(None);
    }

    pi::app::validate_rpc_args(cli)?;

    let mut messages: Vec<String> = cli.message_args().iter().map(ToString::to_string).collect();
    let file_args: Vec<String> = cli.file_args().iter().map(ToString::to_string).collect();
    let initial = pi::app::prepare_initial_message(
        cwd,
        &file_args,
        &mut messages,
        config
            .images
            .as_ref()
            .and_then(|i| i.auto_resize)
            .unwrap_or(true),
    )?;

    let surface_bootstrap = pi::surface::CliSurfaceBootstrap::from_cli(
        cli.mode.as_deref(),
        cli.print,
        cwd.to_path_buf(),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        std::env::args().collect(),
    )?;
    let mode = surface_bootstrap.mode.clone();
    let non_interactive_guard = surface_bootstrap.non_interactive_guard();

    let scoped_patterns = if let Some(models_arg) = &cli.models {
        pi::app::parse_models_arg(models_arg)
    } else {
        config.enabled_models.clone().unwrap_or_default()
    };

    Ok(Some(PreparedSurfaceInputs {
        global_dir,
        models_path,
        model_registry,
        initial,
        messages,
        surface_bootstrap,
        mode,
        non_interactive_guard,
        scoped_patterns,
    }))
}

#[allow(clippy::too_many_lines)]
async fn run(
    mut cli: cli::Cli,
    extension_flags: Vec<cli::ExtensionCliFlag>,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if let Some(command) = cli.command.take() {
        handle_subcommand(command, &cwd).await?;
        return Ok(());
    }

    if !cli.no_migrations {
        let migration_report = pi::migrations::run_startup_migrations(&cwd);
        for message in migration_report.messages() {
            eprintln!("{message}");
        }
    }

    let StartupResources {
        config,
        resources,
        mut auth,
        resource_cli,
    } = load_startup_resources(&cli, &extension_flags, &cwd).await?;
    let extension_runtime_flavor = resolve_extension_runtime_flavor(&resources)?;

    let prewarm_policy = config
        .resolve_extension_policy_with_metadata(cli.extension_policy.as_deref())
        .policy;
    let prewarm_repair = config.resolve_repair_policy_with_metadata(cli.repair_policy.as_deref());
    let prewarm_repair_mode = if prewarm_repair.source == "default" {
        pi::extensions::RepairPolicyMode::AutoStrict
    } else {
        prewarm_repair.effective_mode
    };
    let prewarm_memory_limit_bytes =
        (prewarm_policy.max_memory_mb as usize).saturating_mul(1024 * 1024);

    // Pre-warm extension runtime in a background task so startup work can overlap
    // with auth refresh, model selection, and session creation.
    let extension_prewarm_handle = if matches!(
        extension_runtime_flavor,
        None | Some(ExtensionRuntimeFlavor::Js)
    ) {
        if extension_runtime_flavor.is_none() {
            None
        } else {
            let pre_enabled_tools = cli.enabled_tools();
            let pre_mgr = pi::extensions::ExtensionManager::new();
            pre_mgr.set_cwd(cwd.display().to_string());

            let pre_tools = Arc::new(ToolRegistry::new(&pre_enabled_tools, &cwd, Some(&config)));

            let resolved_risk = config.resolve_extension_risk_with_metadata();
            pre_mgr.set_runtime_risk_config(resolved_risk.settings);

            let pre_mgr_for_runtime = pre_mgr.clone();
            let pre_tools_for_runtime = Arc::clone(&pre_tools);
            let prewarm_policy_for_runtime = prewarm_policy.clone();
            let prewarm_cwd = cwd.display().to_string();
            Some((
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
                    let runtime = JsExtensionRuntimeHandle::start_with_policy(
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
                        stage = "main_prewarm",
                        requested = "quickjs",
                        selected = "quickjs",
                        fallback = false,
                        "Extension runtime engine selected for prewarm (legacy JS/TS)"
                    );
                    Ok::<ExtensionRuntimeHandle, anyhow::Error>(runtime)
                }),
            ))
        }
    } else {
        let pre_enabled_tools = cli.enabled_tools();
        let pre_mgr = pi::extensions::ExtensionManager::new();
        pre_mgr.set_cwd(cwd.display().to_string());
        let pre_tools = Arc::new(ToolRegistry::new(&pre_enabled_tools, &cwd, Some(&config)));

        let resolved_risk = config.resolve_extension_risk_with_metadata();
        pre_mgr.set_runtime_risk_config(resolved_risk.settings);

        Some((
            pre_mgr,
            pre_tools,
            runtime_handle.spawn(async move {
                let runtime = NativeRustExtensionRuntimeHandle::start()
                    .await
                    .map(ExtensionRuntimeHandle::NativeRust)
                    .map_err(anyhow::Error::new)?;
                tracing::info!(
                    event = "pi.extension_runtime.engine_decision",
                    stage = "main_prewarm",
                    requested = "native-rust",
                    selected = "native-rust",
                    fallback = false,
                    "Extension runtime engine selected for prewarm (native-rust)"
                );
                Ok::<ExtensionRuntimeHandle, anyhow::Error>(runtime)
            }),
        ))
    };

    refresh_startup_auth(&mut auth).await?;
    let package_dir = Config::package_dir();
    let Some(PreparedSurfaceInputs {
        global_dir,
        models_path,
        mut model_registry,
        initial,
        messages,
        surface_bootstrap,
        mode,
        non_interactive_guard,
        scoped_patterns,
    }) = prepare_surface_inputs(&mut cli, &config, &auth, &cwd).await?
    else {
        return Ok(());
    };
    let mut scoped_models = if scoped_patterns.is_empty() {
        Vec::new()
    } else {
        pi::app::resolve_model_scope(&scoped_patterns, &model_registry, cli.api_key.is_some())
    };

    if cli.api_key.is_some()
        && cli.provider.is_none()
        && cli.model.is_none()
        && scoped_models.is_empty()
    {
        bail!("--api-key requires a model to be specified via --provider/--model or --models");
    }

    let mut session = Box::pin(Session::new(&cli, &config)).await?;

    let (selection, resolved_key) = loop {
        scoped_models = if scoped_patterns.is_empty() {
            Vec::new()
        } else {
            pi::app::resolve_model_scope(&scoped_patterns, &model_registry, cli.api_key.is_some())
        };

        let selection = match pi::app::select_model_and_thinking(
            &cli,
            &config,
            &session,
            &model_registry,
            &scoped_models,
            &global_dir,
        ) {
            Ok(selection) => selection,
            Err(err) => {
                if let Some(startup) = err.downcast_ref::<StartupError>() {
                    match recover_surface_startup_error_for_selection(
                        &surface_bootstrap,
                        non_interactive_guard.as_ref(),
                        startup,
                        &mut auth,
                        &mut cli,
                        &models_path,
                        &mut model_registry,
                        NonInteractiveStartupRequirement::Tty("first-time setup"),
                    )
                    .await?
                    {
                        SurfaceStartupLoopAction::ContinueSelection => continue,
                        SurfaceStartupLoopAction::ExitQuietly => return Ok(()),
                        SurfaceStartupLoopAction::Propagate => {}
                    }
                }
                return Err(err);
            }
        };

        match pi::app::resolve_api_key(&auth, &cli, &selection.model_entry) {
            Ok(key) => {
                break (selection, key);
            }
            Err(err) => {
                if let Some(startup) = err.downcast_ref::<StartupError>() {
                    if let StartupError::MissingApiKey { provider } = startup {
                        let canonical_provider =
                            pi::provider_metadata::canonical_provider_id(provider)
                                .unwrap_or(provider.as_str());
                        if canonical_provider == "sap-ai-core" {
                            if let Some(token) = pi::auth::exchange_sap_access_token(&auth).await? {
                                break (selection, Some(token));
                            }
                        }
                    }

                    let non_interactive_requirement = match startup {
                        StartupError::MissingApiKey { provider } => {
                            NonInteractiveStartupRequirement::OAuth(provider)
                        }
                        StartupError::NoModelsAvailable { .. } => {
                            NonInteractiveStartupRequirement::None
                        }
                    };

                    match recover_surface_startup_error_for_selection(
                        &surface_bootstrap,
                        non_interactive_guard.as_ref(),
                        startup,
                        &mut auth,
                        &mut cli,
                        &models_path,
                        &mut model_registry,
                        non_interactive_requirement,
                    )
                    .await?
                    {
                        SurfaceStartupLoopAction::ContinueSelection => continue,
                        SurfaceStartupLoopAction::ExitQuietly => return Ok(()),
                        SurfaceStartupLoopAction::Propagate => {}
                    }
                }
                return Err(err);
            }
        }
    };

    pi::app::update_session_for_selection(&mut session, &selection);

    if let Some(message) = &selection.fallback_message {
        eprintln!("Warning: {message}");
    }

    let enabled_tools = cli.enabled_tools();
    let skills_prompt = if enabled_tools.contains(&"read") {
        resources.format_skills_for_prompt()
    } else {
        String::new()
    };
    let test_mode = std::env::var_os("PI_TEST_MODE").is_some();
    let system_prompt = pi::app::build_system_prompt(
        &cli,
        &cwd,
        &enabled_tools,
        if skills_prompt.is_empty() {
            None
        } else {
            Some(skills_prompt.as_str())
        },
        &global_dir,
        &package_dir,
        test_mode,
    );
    let provider =
        providers::create_provider(&selection.model_entry, None).map_err(anyhow::Error::new)?;
    let stream_options = pi::app::build_stream_options(&config, resolved_key, &selection, &session);
    let agent_config = AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations: 50,
        stream_options,
        block_images: config.image_block_images(),
    };

    let tools = ToolRegistry::new(&enabled_tools, &cwd, Some(&config));
    let session_arc = Arc::new(Mutex::new(session));
    let context_window_tokens = if selection.model_entry.model.context_window == 0 {
        tracing::warn!(
            "Model {} reported context_window=0; falling back to default compaction window",
            selection.model_entry.model.id
        );
        ResolvedCompactionSettings::default().context_window_tokens
    } else {
        selection.model_entry.model.context_window
    };
    let compaction_settings = ResolvedCompactionSettings {
        enabled: config.compaction_enabled(),
        reserve_tokens: config.compaction_reserve_tokens(),
        keep_recent_tokens: config.compaction_keep_recent_tokens(),
        context_window_tokens,
    };
    let mut agent_session = AgentSession::new(
        Agent::new(provider, tools, agent_config),
        session_arc,
        !cli.no_session,
        compaction_settings,
    )
    .with_reliability_enabled(config.reliability_enabled())
    .with_reliability_mode(config.reliability_enforcement_mode())
    .with_retry_settings(config.retry.clone().unwrap_or_default());

    restore_session_history(&mut agent_session).await?;

    activate_extensions_for_session(
        &mut agent_session,
        &enabled_tools,
        &cwd,
        &config,
        resources.extensions(),
        &extension_flags,
        &mut auth,
        &mut model_registry,
        cli.extension_policy.as_deref(),
        cli.repair_policy.as_deref(),
        extension_prewarm_handle,
    )
    .await?;

    agent_session.set_model_registry(model_registry.clone());
    agent_session.set_auth_storage(auth.clone());

    // Clone session handle for shutdown flush (ensures autosave queue is drained).
    let session_handle = Arc::clone(&agent_session.session);

    let result = run_selected_surface_route(
        surface_bootstrap.route_kind,
        agent_session,
        &selection,
        model_registry.get_available(),
        initial,
        messages,
        resources,
        &config,
        auth.clone(),
        runtime_handle.clone(),
        resource_cli,
        cwd.clone(),
        !cli.no_session,
        &mode,
    )
    .await;

    flush_session_autosave_on_shutdown(&session_handle, cli.no_session).await;

    result
}

async fn handle_subcommand(command: cli::Commands, cwd: &Path) -> Result<()> {
    let manager = PackageManager::new(cwd.to_path_buf());
    match command {
        cli::Commands::Install { source, local } => {
            handle_package_install(&manager, &source, local).await?;
        }
        cli::Commands::Remove { source, local } => {
            handle_package_remove(&manager, &source, local).await?;
        }
        cli::Commands::Update { source } => {
            handle_package_update(&manager, source).await?;
        }
        cli::Commands::UpdateIndex => {
            handle_update_index().await?;
        }
        cli::Commands::Search {
            query,
            tag,
            sort,
            limit,
        } => {
            handle_search(&query, tag.as_deref(), &sort, limit).await?;
        }
        cli::Commands::Info { name } => {
            handle_info(&name).await?;
        }
        cli::Commands::List => {
            handle_package_list(&manager).await?;
        }
        cli::Commands::Config { show, paths, json } => {
            handle_config(&manager, cwd, show, paths, json).await?;
        }
        cli::Commands::Doctor {
            path,
            format,
            policy,
            fix,
            only,
        } => {
            handle_doctor(
                cwd,
                path.as_deref(),
                &format,
                policy.as_deref(),
                fix,
                only.as_deref(),
            )?;
        }
        cli::Commands::Migrate { path, dry_run } => {
            handle_session_migrate(&path, dry_run)?;
        }
        cli::Commands::Account { command } => {
            handle_account_command(command)?;
        }
    }

    Ok(())
}

fn spawn_session_index_maintenance() {
    const MAX_INDEX_AGE: Duration = Duration::from_secs(60 * 30);
    let index = SessionIndex::new();

    // Always spawn the background thread to handle cleanup, regardless of reindexing needs.
    // Cleanup can be slow if there are many temp files, so we don't want to block main.
    std::thread::spawn(move || {
        // Clean up old bash tool logs in background
        pi::tools::cleanup_temp_files();

        if index.should_reindex(MAX_INDEX_AGE) {
            if let Err(err) = index.reindex_all() {
                eprintln!("Warning: failed to reindex session index: {err}");
            }
        }
    });
}

async fn flush_session_autosave_on_shutdown(session_handle: &Arc<Mutex<Session>>, disabled: bool) {
    if disabled {
        return;
    }

    let cx = pi::agent_cx::AgentCx::for_request();
    if let Ok(mut guard) = session_handle.lock(cx.cx()).await {
        if let Err(e) = guard.flush_autosave_on_shutdown().await {
            eprintln!("Warning: Failed to flush session autosave: {e}");
        }
    }
}

async fn restore_session_history(agent_session: &mut AgentSession) -> Result<()> {
    let history = {
        let cx = pi::agent_cx::AgentCx::for_request();
        let session = agent_session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        session.to_messages_for_current_path()
    };

    if !history.is_empty() {
        agent_session.agent.replace_messages(history);
    }

    Ok(())
}

const fn scope_from_flag(local: bool) -> PackageScope {
    if local {
        PackageScope::Project
    } else {
        PackageScope::User
    }
}

async fn handle_package_install(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    let resolved_source = manager.resolve_install_source_alias(source);
    manager.install(&resolved_source, scope).await?;
    manager.add_package_source(&resolved_source, scope).await?;
    if resolved_source == source {
        println!("Installed {source}");
    } else {
        println!("Installed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

async fn handle_package_remove(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    let resolved_source = manager.resolve_install_source_alias(source);
    manager.remove(&resolved_source, scope).await?;
    manager
        .remove_package_source(&resolved_source, scope)
        .await?;
    if resolved_source == source {
        println!("Removed {source}");
    } else {
        println!("Removed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

async fn handle_package_update(manager: &PackageManager, source: Option<String>) -> Result<()> {
    let entries = manager.list_packages().await?;

    if let Some(source) = source {
        let resolved_source = manager.resolve_install_source_alias(&source);
        let identity = manager.package_identity(&resolved_source);
        for entry in entries {
            if manager.package_identity(&entry.source) != identity {
                continue;
            }
            manager.update_source(&entry.source, entry.scope).await?;
        }
        if resolved_source == source {
            println!("Updated {source}");
        } else {
            println!("Updated {source} (resolved to {resolved_source})");
        }
        return Ok(());
    }

    for entry in entries {
        manager.update_source(&entry.source, entry.scope).await?;
    }
    println!("Updated packages");
    Ok(())
}

async fn handle_package_list(manager: &PackageManager) -> Result<()> {
    let entries = manager.list_packages().await?;
    let (user, project) = split_package_entries(entries);

    if user.is_empty() && project.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }

    if !user.is_empty() {
        println!("User packages:");
        for entry in &user {
            print_package_entry(manager, entry).await?;
        }
    }

    if !project.is_empty() {
        if !user.is_empty() {
            println!();
        }
        println!("Project packages:");
        for entry in &project {
            print_package_entry(manager, entry).await?;
        }
    }

    Ok(())
}

fn handle_package_list_blocking(manager: &PackageManager) -> Result<()> {
    let entries = manager.list_packages_blocking()?;
    print_package_list_entries_blocking(manager, entries, print_package_entry_blocking)
}

fn split_package_entries(entries: Vec<PackageEntry>) -> (Vec<PackageEntry>, Vec<PackageEntry>) {
    let mut user = Vec::new();
    let mut project = Vec::new();
    for entry in entries {
        match entry.scope {
            PackageScope::User => user.push(entry),
            PackageScope::Project | PackageScope::Temporary => project.push(entry),
        }
    }
    (user, project)
}

fn print_package_list_entries_blocking<F>(
    manager: &PackageManager,
    entries: Vec<PackageEntry>,
    mut print_entry: F,
) -> Result<()>
where
    F: FnMut(&PackageManager, &PackageEntry) -> Result<()>,
{
    let (user, project) = split_package_entries(entries);

    if user.is_empty() && project.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }

    if !user.is_empty() {
        println!("User packages:");
        for entry in &user {
            print_entry(manager, entry)?;
        }
    }

    if !project.is_empty() {
        if !user.is_empty() {
            println!();
        }
        println!("Project packages:");
        for entry in &project {
            print_entry(manager, entry)?;
        }
    }

    Ok(())
}

async fn handle_update_index() -> Result<()> {
    let store = ExtensionIndexStore::default_store();
    let client = pi::http::client::Client::new();
    let (_, stats) = store.refresh_best_effort(&client).await?;

    if !stats.refreshed {
        println!(
            "Extension index refresh skipped: remote sources unavailable; using existing seed/cache."
        );
        return Ok(());
    }

    println!(
        "Extension index refreshed: {} merged entries (npm: {}, github: {}) at {}",
        stats.merged_entries,
        stats.npm_entries,
        stats.github_entries,
        store.path().display()
    );
    Ok(())
}

async fn handle_search(query: &str, tag: Option<&str>, sort: &str, limit: usize) -> Result<()> {
    let store = ExtensionIndexStore::default_store();

    // Load cached index; auto-refresh only if a cache file exists but is stale.
    // If no cache exists, use the built-in seed index without a network call.
    let mut index = store.load_or_seed()?;
    let has_cache = store.path().exists();
    if has_cache
        && index.is_stale(
            chrono::Utc::now(),
            pi::extension_index::DEFAULT_INDEX_MAX_AGE,
        )
    {
        println!("Refreshing extension index...");
        let client = pi::http::client::Client::new();
        match store.refresh_best_effort(&client).await {
            Ok((refreshed, _)) => index = refreshed,
            Err(_) => {
                println!(
                    "Warning: Could not refresh index (network unavailable). Using cached results."
                );
            }
        }
    }

    render_search_results(&index, query, tag, sort, limit);
    Ok(())
}

fn handle_search_blocking(
    query: &str,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
) -> Result<bool> {
    let store = ExtensionIndexStore::default_store();
    let index = store.load_or_seed()?;

    // Preserve refresh semantics: if cache is stale, fall back to async path so we can
    // attempt network refresh before searching.
    let has_cache = store.path().exists();
    if has_cache
        && index.is_stale(
            chrono::Utc::now(),
            pi::extension_index::DEFAULT_INDEX_MAX_AGE,
        )
    {
        return Ok(false);
    }

    render_search_results(&index, query, tag, sort, limit);
    Ok(true)
}

fn render_search_results(
    index: &pi::extension_index::ExtensionIndex,
    query: &str,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
) {
    let hits = collect_search_hits(index, tag, sort, limit, query);
    if hits.is_empty() {
        println!("No extensions found for \"{query}\".");
        return;
    }

    print_search_results(&hits);
}

fn collect_search_hits(
    index: &pi::extension_index::ExtensionIndex,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
    query: &str,
) -> Vec<pi::extension_index::ExtensionSearchHit> {
    let mut hits = index.search(query, limit);

    // Filter by tag if requested
    if let Some(tag_filter) = tag {
        let tag_lower = tag_filter.to_ascii_lowercase();
        hits.retain(|hit| {
            hit.entry
                .tags
                .iter()
                .any(|t| t.to_ascii_lowercase() == tag_lower)
        });
    }

    // Sort by name if requested (relevance is the default from search())
    if sort == "name" {
        hits.sort_by(|a, b| {
            a.entry
                .name
                .to_ascii_lowercase()
                .cmp(&b.entry.name.to_ascii_lowercase())
        });
    }

    hits
}

#[allow(clippy::uninlined_format_args)]
fn print_search_results(hits: &[pi::extension_index::ExtensionSearchHit]) {
    // Column widths
    let name_w = hits
        .iter()
        .map(|h| h.entry.name.len())
        .max()
        .unwrap_or(0)
        .max(4); // "Name"
    let desc_w = hits
        .iter()
        .map(|h| h.entry.description.as_deref().unwrap_or("").len().min(50))
        .max()
        .unwrap_or(0)
        .max(11); // "Description"
    let tags_w = hits
        .iter()
        .map(|h| h.entry.tags.join(", ").len().min(30))
        .max()
        .unwrap_or(0)
        .max(4); // "Tags"
    let source_w = 6; // "Source"

    // Header
    println!(
        "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
        "Name", "Description", "Tags", "Source"
    );
    println!(
        "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
        "-".repeat(name_w),
        "-".repeat(desc_w),
        "-".repeat(tags_w),
        "-".repeat(source_w)
    );

    // Rows
    for hit in hits {
        let desc = hit.entry.description.as_deref().unwrap_or("");
        let desc_truncated = if desc.chars().count() > 50 {
            let truncated: String = desc.chars().take(47).collect();
            format!("{truncated}...")
        } else {
            desc.to_string()
        };
        let tags_joined = hit.entry.tags.join(", ");
        let tags_truncated = if tags_joined.chars().count() > 30 {
            let truncated: String = tags_joined.chars().take(27).collect();
            format!("{truncated}...")
        } else {
            tags_joined
        };
        let source_label = match &hit.entry.source {
            Some(pi::extension_index::ExtensionIndexSource::Npm { .. }) => "npm",
            Some(pi::extension_index::ExtensionIndexSource::Git { .. }) => "git",
            Some(pi::extension_index::ExtensionIndexSource::Url { .. }) => "url",
            None => "-",
        };
        println!(
            "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
            hit.entry.name, desc_truncated, tags_truncated, source_label
        );
    }

    let count = hits.len();
    let noun = if count == 1 {
        "extension"
    } else {
        "extensions"
    };
    println!("\n  {count} {noun} found. Install with: pi install <name>");
}

async fn handle_info(name: &str) -> Result<()> {
    handle_info_blocking(name)
}

fn handle_info_blocking(name: &str) -> Result<()> {
    let index = ExtensionIndexStore::default_store().load_or_seed()?;
    let entry = find_index_entry_by_name_or_id(&index, name);
    let Some(entry) = entry else {
        println!("Extension \"{name}\" not found.");
        println!("Try: pi search {name}");
        return Ok(());
    };
    print_extension_info(entry);
    Ok(())
}

fn find_index_entry_by_name_or_id<'a>(
    index: &'a pi::extension_index::ExtensionIndex,
    name: &str,
) -> Option<&'a pi::extension_index::ExtensionIndexEntry> {
    // Look up by exact id, name, or fuzzy match (top-1 search hit)
    index
        .entries
        .iter()
        .find(|e| e.id.eq_ignore_ascii_case(name) || e.name.eq_ignore_ascii_case(name))
        .or_else(|| {
            let hits = index.search(name, 1);
            hits.into_iter()
                .next()
                .map(|h| h.entry)
                .and_then(|matched| {
                    // Return a reference from the index, not the owned clone
                    index.entries.iter().find(|e| e.id == matched.id)
                })
        })
}

fn print_extension_info(entry: &pi::extension_index::ExtensionIndexEntry) {
    let width = 60;
    let bar = "─".repeat(width);

    // Header
    println!("  ┌{bar}┐");
    let title = &entry.name;
    let padding = width.saturating_sub(title.len() + 1);
    println!("  │ {title}{:padding$}│", "");

    // ID (if different from name)
    if entry.id != entry.name {
        let id_line = format!("id: {}", entry.id);
        let padding = width.saturating_sub(id_line.len() + 1);
        println!("  │ {id_line}{:padding$}│", "");
    }

    // Description
    if let Some(desc) = &entry.description {
        println!("  │{:width$}│", "");
        for line in wrap_text(desc, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    }

    // Separator
    println!("  ├{bar}┤");

    // Tags
    if !entry.tags.is_empty() {
        let tags_line = format!("Tags: {}", entry.tags.join(", "));
        let padding = width.saturating_sub(tags_line.len() + 1);
        println!("  │ {tags_line}{:padding$}│", "");
    }

    // License
    if let Some(license) = &entry.license {
        let lic_line = format!("License: {license}");
        let padding = width.saturating_sub(lic_line.len() + 1);
        println!("  │ {lic_line}{:padding$}│", "");
    }

    // Source
    if let Some(source) = &entry.source {
        let source_line = match source {
            pi::extension_index::ExtensionIndexSource::Npm {
                package, version, ..
            } => {
                let ver = version.as_deref().unwrap_or("latest");
                format!("Source: npm:{package}@{ver}")
            }
            pi::extension_index::ExtensionIndexSource::Git { repo, path, .. } => {
                let suffix = path.as_deref().map_or(String::new(), |p| format!(" ({p})"));
                format!("Source: git:{repo}{suffix}")
            }
            pi::extension_index::ExtensionIndexSource::Url { url } => {
                format!("Source: {url}")
            }
        };
        for line in wrap_text(&source_line, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    }

    // Install command
    println!("  ├{bar}┤");
    if let Some(install_source) = &entry.install_source {
        let install_line = format!("Install: pi install {install_source}");
        for line in wrap_text(&install_line, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    } else {
        let hint = "Install source not available";
        let padding = width.saturating_sub(hint.len() + 1);
        println!("  │ {hint}{:padding$}│", "");
    }

    println!("  └{bar}┘");
}

/// Wrap text to fit within `max_width` characters.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current = word.to_string();
            } else if current.len() + 1 + word.len() <= max_width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

async fn print_package_entry(manager: &PackageManager, entry: &PackageEntry) -> Result<()> {
    let display = if entry.filter.is_some() {
        format!("{} (filtered)", entry.source)
    } else {
        entry.source.clone()
    };
    println!("  {display}");
    if let Some(path) = manager.installed_path(&entry.source, entry.scope).await? {
        println!("    {}", path.display());
    }
    Ok(())
}

fn print_package_entry_blocking(manager: &PackageManager, entry: &PackageEntry) -> Result<()> {
    let display = if entry.filter.is_some() {
        format!("{} (filtered)", entry.source)
    } else {
        entry.source.clone()
    };
    println!("  {display}");
    if let Some(path) = manager.installed_path_blocking(&entry.source, entry.scope)? {
        println!("    {}", path.display());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ConfigResourceKind {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ConfigResourceKind {
    const ALL: [Self; 4] = [Self::Extensions, Self::Skills, Self::Prompts, Self::Themes];

    const fn field_name(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Extensions => "extension",
            Self::Skills => "skill",
            Self::Prompts => "prompt",
            Self::Themes => "theme",
        }
    }

    const fn order(self) -> usize {
        match self {
            Self::Extensions => 0,
            Self::Skills => 1,
            Self::Prompts => 2,
            Self::Themes => 3,
        }
    }
}

#[derive(Debug, Clone)]
struct ConfigResourceState {
    kind: ConfigResourceKind,
    path: String,
    enabled: bool,
}

#[derive(Debug, Clone)]
struct ConfigPackageState {
    scope: SettingsScope,
    source: String,
    resources: Vec<ConfigResourceState>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPathsReport {
    global: String,
    project: String,
    auth: String,
    sessions: String,
    packages: String,
    extension_index: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigResourceReport {
    kind: String,
    path: String,
    enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPackageReport {
    scope: String,
    source: String,
    resources: Vec<ConfigResourceReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigReport {
    paths: ConfigPathsReport,
    precedence: Vec<String>,
    config_valid: bool,
    config_error: Option<String>,
    packages: Vec<ConfigPackageReport>,
}

#[derive(Debug, Clone, Default)]
struct PackageFilterState {
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PackageFilterState {
    fn set_kind(&mut self, kind: ConfigResourceKind, values: Vec<String>) {
        match kind {
            ConfigResourceKind::Extensions => self.extensions = Some(values),
            ConfigResourceKind::Skills => self.skills = Some(values),
            ConfigResourceKind::Prompts => self.prompts = Some(values),
            ConfigResourceKind::Themes => self.themes = Some(values),
        }
    }

    const fn values_for_kind(&self, kind: ConfigResourceKind) -> Option<&Vec<String>> {
        match kind {
            ConfigResourceKind::Extensions => self.extensions.as_ref(),
            ConfigResourceKind::Skills => self.skills.as_ref(),
            ConfigResourceKind::Prompts => self.prompts.as_ref(),
            ConfigResourceKind::Themes => self.themes.as_ref(),
        }
    }

    const fn has_any_field(&self) -> bool {
        self.extensions.is_some()
            || self.skills.is_some()
            || self.prompts.is_some()
            || self.themes.is_some()
    }
}

#[derive(Debug, Clone)]
struct ConfigUiResult {
    save_requested: bool,
    packages: Vec<ConfigPackageState>,
}

#[derive(bubbletea::Model)]
struct ConfigUiApp {
    packages: Vec<ConfigPackageState>,
    selected: usize,
    settings_summary: String,
    status: String,
    result_slot: Arc<StdMutex<Option<ConfigUiResult>>>,
}

impl ConfigUiApp {
    fn new(
        packages: Vec<ConfigPackageState>,
        settings_summary: String,
        result_slot: Arc<StdMutex<Option<ConfigUiResult>>>,
    ) -> Self {
        let status = if packages.iter().any(|pkg| !pkg.resources.is_empty()) {
            String::new()
        } else {
            "No package resources discovered. Press Enter to exit.".to_string()
        };

        Self {
            packages,
            selected: 0,
            settings_summary,
            status,
            result_slot,
        }
    }

    fn selectable_count(&self) -> usize {
        self.packages.iter().map(|pkg| pkg.resources.len()).sum()
    }

    fn selected_coords(&self) -> Option<(usize, usize)> {
        let mut cursor = 0usize;
        for (pkg_idx, pkg) in self.packages.iter().enumerate() {
            for (res_idx, _) in pkg.resources.iter().enumerate() {
                if cursor == self.selected {
                    return Some((pkg_idx, res_idx));
                }
                cursor = cursor.saturating_add(1);
            }
        }
        None
    }

    fn move_selection(&mut self, delta: isize) {
        let total = self.selectable_count();
        if total == 0 {
            self.selected = 0;
            return;
        }

        let max_index = total.saturating_sub(1);
        let step = delta.unsigned_abs();
        if delta.is_negative() {
            self.selected = self.selected.saturating_sub(step);
        } else {
            self.selected = self.selected.saturating_add(step).min(max_index);
        }
    }

    fn toggle_selected(&mut self) {
        if let Some((pkg_idx, res_idx)) = self.selected_coords() {
            if let Some(resource) = self
                .packages
                .get_mut(pkg_idx)
                .and_then(|pkg| pkg.resources.get_mut(res_idx))
            {
                resource.enabled = !resource.enabled;
            }
        }
    }

    fn finish(&self, save_requested: bool) -> Cmd {
        if let Ok(mut slot) = self.result_slot.lock() {
            *slot = Some(ConfigUiResult {
                save_requested,
                packages: self.packages.clone(),
            });
        }
        quit()
    }

    #[allow(clippy::missing_const_for_fn, clippy::unused_self)]
    fn init(&self) -> Option<Cmd> {
        None
    }

    #[allow(clippy::needless_pass_by_value)]
    fn update(&mut self, msg: BubbleMessage) -> Option<Cmd> {
        if let Some(key) = msg.downcast_ref::<KeyMsg>() {
            match key.key_type {
                KeyType::Up => self.move_selection(-1),
                KeyType::Down => self.move_selection(1),
                KeyType::Runes if key.runes == ['k'] => self.move_selection(-1),
                KeyType::Runes if key.runes == ['j'] => self.move_selection(1),
                KeyType::Space => self.toggle_selected(),
                KeyType::Enter => return Some(self.finish(true)),
                KeyType::Esc | KeyType::CtrlC => return Some(self.finish(false)),
                KeyType::Runes if key.runes == ['q'] => return Some(self.finish(false)),
                _ => {}
            }
        }
        None
    }

    fn view(&self) -> String {
        let mut out = String::new();
        out.push_str("Pi Config UI\n");
        let _ = writeln!(out, "{}", self.settings_summary);
        out.push_str("Keys: ↑/↓ (or j/k) move, Space toggle, Enter save, q cancel\n\n");

        let mut cursor = 0usize;
        for package in &self.packages {
            let _ = writeln!(
                out,
                "{} package: {}",
                scope_label(package.scope),
                package.source
            );

            if package.resources.is_empty() {
                out.push_str("    (no discovered resources)\n");
                continue;
            }

            for resource in &package.resources {
                let selected = cursor == self.selected;
                let marker = if resource.enabled { "x" } else { " " };
                let prefix = if selected { ">" } else { " " };
                let _ = writeln!(
                    out,
                    "{} [{}] {:<10} {}",
                    prefix,
                    marker,
                    resource.kind.label(),
                    resource.path
                );
                cursor = cursor.saturating_add(1);
            }

            out.push('\n');
        }

        if !self.status.is_empty() {
            let _ = writeln!(out, "{}", self.status);
        }

        out
    }
}

const fn scope_label(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "Global",
        SettingsScope::Project => "Project",
    }
}

const fn scope_key(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "global",
        SettingsScope::Project => "project",
    }
}

const fn settings_scope_from_package_scope(scope: PackageScope) -> Option<SettingsScope> {
    match scope {
        PackageScope::User => Some(SettingsScope::Global),
        PackageScope::Project => Some(SettingsScope::Project),
        PackageScope::Temporary => None,
    }
}

fn package_lookup_key(scope: SettingsScope, source: &str) -> String {
    format!("{}::{source}", scope_key(scope))
}

fn normalize_path_for_display(path: &Path, base_dir: Option<&Path>) -> String {
    let rel = base_dir
        .and_then(|base| path.strip_prefix(base).ok())
        .unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

fn normalize_filter_entry(path: &str) -> String {
    path.replace('\\', "/")
}

fn merge_resolved_resources(
    kind: ConfigResourceKind,
    resources: &[ResolvedResource],
    packages: &mut Vec<ConfigPackageState>,
    lookup: &mut std::collections::HashMap<String, usize>,
) {
    for resource in resources {
        if resource.metadata.origin != ResourceOrigin::Package {
            continue;
        }

        let Some(scope) = settings_scope_from_package_scope(resource.metadata.scope) else {
            continue;
        };

        let key = package_lookup_key(scope, &resource.metadata.source);
        let idx = lookup.get(&key).copied().unwrap_or_else(|| {
            let idx = packages.len();
            packages.push(ConfigPackageState {
                scope,
                source: resource.metadata.source.clone(),
                resources: Vec::new(),
            });
            lookup.insert(key, idx);
            idx
        });

        let path =
            normalize_path_for_display(&resource.path, resource.metadata.base_dir.as_deref());
        packages[idx].resources.push(ConfigResourceState {
            kind,
            path,
            enabled: resource.enabled,
        });
    }
}

fn sort_and_dedupe_package_resources(packages: &mut [ConfigPackageState]) {
    for package in packages {
        package.resources.sort_by(|a, b| {
            (a.kind.order(), a.path.as_str()).cmp(&(b.kind.order(), b.path.as_str()))
        });

        let mut deduped: Vec<ConfigResourceState> = Vec::new();
        for resource in std::mem::take(&mut package.resources) {
            if let Some(existing) = deduped
                .iter_mut()
                .find(|r| r.kind == resource.kind && r.path == resource.path)
            {
                existing.enabled = existing.enabled || resource.enabled;
            } else {
                deduped.push(resource);
            }
        }
        package.resources = deduped;
    }
}

async fn collect_config_packages(manager: &PackageManager) -> Vec<ConfigPackageState> {
    let mut packages = Vec::new();
    let mut lookup = std::collections::HashMap::<String, usize>::new();

    let entries = manager.list_packages().await.unwrap_or_default();
    for entry in entries {
        let Some(scope) = settings_scope_from_package_scope(entry.scope) else {
            continue;
        };
        let key = package_lookup_key(scope, &entry.source);
        if lookup.contains_key(&key) {
            continue;
        }
        lookup.insert(key, packages.len());
        packages.push(ConfigPackageState {
            scope,
            source: entry.source,
            resources: Vec::new(),
        });
    }

    match manager.resolve().await {
        Ok(ResolvedPaths {
            extensions,
            skills,
            prompts,
            themes,
        }) => {
            merge_resolved_resources(
                ConfigResourceKind::Extensions,
                &extensions,
                &mut packages,
                &mut lookup,
            );
            merge_resolved_resources(
                ConfigResourceKind::Skills,
                &skills,
                &mut packages,
                &mut lookup,
            );
            merge_resolved_resources(
                ConfigResourceKind::Prompts,
                &prompts,
                &mut packages,
                &mut lookup,
            );
            merge_resolved_resources(
                ConfigResourceKind::Themes,
                &themes,
                &mut packages,
                &mut lookup,
            );
        }
        Err(err) => {
            eprintln!("Warning: failed to resolve package resources for config UI: {err}");
        }
    }

    sort_and_dedupe_package_resources(&mut packages);
    packages
}

fn build_config_report(cwd: &Path, packages: &[ConfigPackageState]) -> ConfigReport {
    let config_path = std::env::var("PI_CONFIG_PATH")
        .ok()
        .map_or_else(|| Config::global_dir().join("settings.json"), PathBuf::from);
    let project_path = cwd.join(Config::project_dir()).join("settings.json");

    let (config_valid, config_error) = match Config::load() {
        Ok(_) => (true, None),
        Err(err) => (false, Some(err.to_string())),
    };

    let packages = packages
        .iter()
        .map(|package| ConfigPackageReport {
            scope: scope_key(package.scope).to_string(),
            source: package.source.clone(),
            resources: package
                .resources
                .iter()
                .map(|resource| ConfigResourceReport {
                    kind: resource.kind.field_name().to_string(),
                    path: resource.path.clone(),
                    enabled: resource.enabled,
                })
                .collect(),
        })
        .collect::<Vec<_>>();

    ConfigReport {
        paths: ConfigPathsReport {
            global: config_path.display().to_string(),
            project: project_path.display().to_string(),
            auth: Config::auth_path().display().to_string(),
            sessions: Config::sessions_dir().display().to_string(),
            packages: Config::package_dir().display().to_string(),
            extension_index: Config::extension_index_path().display().to_string(),
        },
        precedence: vec![
            "CLI flags".to_string(),
            "Environment variables".to_string(),
            format!("Project settings ({})", project_path.display()),
            format!("Global settings ({})", config_path.display()),
            "Built-in defaults".to_string(),
        ],
        config_valid,
        config_error,
        packages,
    }
}

fn print_config_report(report: &ConfigReport, include_packages: bool) {
    println!("Settings paths:");
    println!("  Global:  {}", report.paths.global);
    println!("  Project: {}", report.paths.project);
    println!();
    println!("Other paths:");
    println!("  Auth:     {}", report.paths.auth);
    println!("  Sessions: {}", report.paths.sessions);
    println!("  Packages: {}", report.paths.packages);
    println!("  ExtIndex: {}", report.paths.extension_index);
    println!();
    println!("Settings precedence:");
    for (idx, entry) in report.precedence.iter().enumerate() {
        println!("  {}) {}", idx + 1, entry);
    }
    println!();

    if report.config_valid {
        println!("Current configuration is valid.");
    } else if let Some(error) = &report.config_error {
        println!("Configuration Error: {error}");
    }

    if !include_packages {
        return;
    }

    println!();
    println!("Package resources:");
    if report.packages.is_empty() {
        println!("  (no configured packages)");
        return;
    }

    for package in &report.packages {
        println!("  [{}] {}", package.scope, package.source);
        if package.resources.is_empty() {
            println!("    (no discovered resources)");
            continue;
        }
        for resource in &package.resources {
            let marker = if resource.enabled { "x" } else { " " };
            println!("    [{}] {:<10} {}", marker, resource.kind, resource.path);
        }
    }
}

fn format_settings_summary(config: &Config) -> String {
    let provider = config.default_provider.as_deref().unwrap_or("(default)");
    let model = config.default_model.as_deref().unwrap_or("(default)");
    let thinking = config
        .default_thinking_level
        .as_deref()
        .unwrap_or("(default)");
    format!("provider={provider}  model={model}  thinking={thinking}")
}

fn run_config_tui(
    packages: Vec<ConfigPackageState>,
    settings_summary: String,
) -> Result<Option<Vec<ConfigPackageState>>> {
    let result_slot = Arc::new(StdMutex::new(None));
    let app = ConfigUiApp::new(packages, settings_summary, Arc::clone(&result_slot));
    Program::new(app).with_alt_screen().run()?;

    let result = result_slot.lock().ok().and_then(|guard| guard.clone());
    match result {
        Some(result) if result.save_requested => Ok(Some(result.packages)),
        _ => Ok(None),
    }
}

fn load_settings_json_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }

    let content = std::fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(json!({}));
    }

    let value: Value = serde_json::from_str(&content)?;
    if value.is_object() {
        Ok(value)
    } else {
        Ok(json!({}))
    }
}

fn extract_package_source(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string).or_else(|| {
        value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn persist_package_toggles(cwd: &Path, packages: &[ConfigPackageState]) -> Result<()> {
    let global_dir = Config::global_dir();
    persist_package_toggles_with_roots(cwd, &global_dir, packages)
}

#[allow(clippy::too_many_lines)]
fn persist_package_toggles_with_roots(
    cwd: &Path,
    global_dir: &Path,
    packages: &[ConfigPackageState],
) -> Result<()> {
    let mut updates_by_scope: std::collections::HashMap<
        SettingsScope,
        std::collections::HashMap<String, PackageFilterState>,
    > = std::collections::HashMap::new();

    for package in packages {
        if package.resources.is_empty() {
            continue;
        }

        let mut state = PackageFilterState::default();
        for kind in ConfigResourceKind::ALL {
            let kind_resources = package
                .resources
                .iter()
                .filter(|resource| resource.kind == kind)
                .collect::<Vec<_>>();
            if kind_resources.is_empty() {
                continue;
            }

            let mut enabled = kind_resources
                .iter()
                .filter(|resource| resource.enabled)
                .map(|resource| normalize_filter_entry(&resource.path))
                .collect::<Vec<_>>();
            enabled.sort();
            enabled.dedup();
            state.set_kind(kind, enabled);
        }

        if !state.has_any_field() {
            continue;
        }

        updates_by_scope
            .entry(package.scope)
            .or_default()
            .insert(package.source.clone(), state);
    }

    for scope in [SettingsScope::Global, SettingsScope::Project] {
        let Some(scope_updates) = updates_by_scope.get(&scope) else {
            continue;
        };

        let settings_path = Config::settings_path_with_roots(scope, global_dir, cwd);
        let mut settings = load_settings_json_object(&settings_path)?;
        if !settings.is_object() {
            settings = json!({});
        }

        let packages_array = settings
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("settings should be an object"))?
            .entry("packages".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !packages_array.is_array() {
            *packages_array = Value::Array(Vec::new());
        }

        let package_entries = packages_array
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("packages should be an array"))?;

        let mut updated_sources = std::collections::HashSet::new();
        for entry in package_entries.iter_mut() {
            let Some(source) = extract_package_source(entry) else {
                continue;
            };
            let Some(filter_state) = scope_updates.get(&source) else {
                continue;
            };

            let mut obj = entry
                .as_object()
                .cloned()
                .unwrap_or_else(serde_json::Map::new);
            obj.insert("source".to_string(), Value::String(source.clone()));
            for kind in ConfigResourceKind::ALL {
                if let Some(values) = filter_state.values_for_kind(kind) {
                    let arr = values
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>();
                    obj.insert(kind.field_name().to_string(), Value::Array(arr));
                }
            }
            *entry = Value::Object(obj);
            updated_sources.insert(source);
        }

        for (source, filter_state) in scope_updates {
            if updated_sources.contains(source) {
                continue;
            }

            let mut obj = serde_json::Map::new();
            obj.insert("source".to_string(), Value::String(source.clone()));
            for kind in ConfigResourceKind::ALL {
                if let Some(values) = filter_state.values_for_kind(kind) {
                    let arr = values
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>();
                    obj.insert(kind.field_name().to_string(), Value::Array(arr));
                }
            }
            package_entries.push(Value::Object(obj));
        }

        let patch = json!({ "packages": package_entries.clone() });
        Config::patch_settings_with_roots(scope, global_dir, cwd, patch)?;
    }

    Ok(())
}

async fn handle_config(
    manager: &PackageManager,
    cwd: &Path,
    show: bool,
    paths: bool,
    json_output: bool,
) -> Result<()> {
    if json_output && (show || paths) {
        bail!("`pi config --json` cannot be combined with --show/--paths");
    }

    let packages = collect_config_packages(manager).await;
    let report = build_config_report(cwd, &packages);

    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let interactive_requested = !show && !paths;
    let has_tty = io::stdin().is_terminal() && io::stdout().is_terminal();

    if interactive_requested && has_tty {
        let config = Config::load().unwrap_or_default();
        let settings_summary = format_settings_summary(&config);
        if let Some(updated) = run_config_tui(packages, settings_summary)? {
            persist_package_toggles(cwd, &updated)?;
            println!("Saved package resource toggles.");
        } else {
            println!("No changes saved.");
        }
        return Ok(());
    }

    print_config_report(&report, show);
    Ok(())
}

fn collect_session_jsonl_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }

    files.sort();
    Ok(files)
}

fn handle_session_migrate(path: &str, dry_run: bool) -> Result<()> {
    let path = std::path::Path::new(path);
    if !path.exists() {
        bail!("Path does not exist: {}", path.display());
    }

    // Collect JSONL files to migrate.
    let jsonl_files: Vec<std::path::PathBuf> = if path.is_dir() {
        let files = collect_session_jsonl_files(path)?;
        if files.is_empty() {
            bail!("No .jsonl session files found in {}", path.display());
        }
        files
    } else {
        vec![path.to_path_buf()]
    };

    let mut migrated = 0u64;
    let mut errors = 0u64;

    for jsonl_path in &jsonl_files {
        if dry_run {
            match pi::session::migrate_dry_run(jsonl_path) {
                Ok(verification) => {
                    let status = if verification.entry_count_match
                        && verification.hash_chain_match
                        && verification.index_consistent
                    {
                        "OK"
                    } else {
                        "MISMATCH"
                    };
                    println!(
                        "[dry-run] {}: {} (entries_match={}, hash_match={}, index_ok={})",
                        jsonl_path.display(),
                        status,
                        verification.entry_count_match,
                        verification.hash_chain_match,
                        verification.index_consistent,
                    );
                    migrated += 1;
                }
                Err(e) => {
                    eprintln!("[dry-run] {}: ERROR: {e}", jsonl_path.display());
                    errors += 1;
                }
            }
        } else {
            let correlation_id = uuid::Uuid::new_v4().to_string();
            match pi::session::migrate_jsonl_to_v2(jsonl_path, &correlation_id) {
                Ok(event) => {
                    println!(
                        "[migrated] {}: migration_id={}, entries_match={}, hash_match={}, index_ok={}",
                        jsonl_path.display(),
                        event.migration_id,
                        event.verification.entry_count_match,
                        event.verification.hash_chain_match,
                        event.verification.index_consistent,
                    );
                    migrated += 1;
                }
                Err(e) => {
                    eprintln!("[error] {}: {e}", jsonl_path.display());
                    errors += 1;
                }
            }
        }
    }

    println!(
        "\nSession migration complete: {migrated} succeeded, {errors} failed (dry_run={dry_run})"
    );
    if errors > 0 {
        bail!("{errors} session(s) failed migration");
    }
    Ok(())
}

/// Handle account management commands.
fn handle_account_command(command: cli::AccountCommands) -> Result<()> {
    use cli::AccountCommands;

    let auth_path = pi::config::Config::auth_path();

    fn parse_revocation_reason(reason: Option<String>) -> pi::auth::RevocationReason {
        match reason
            .as_deref()
            .map_or("user_requested", str::trim)
            .to_ascii_lowercase()
            .as_str()
        {
            "compromised" => pi::auth::RevocationReason::Compromised,
            "auth_failure" | "auth-failure" => pi::auth::RevocationReason::AuthFailure,
            "provider_invalidated" | "provider-invalidated" => {
                pi::auth::RevocationReason::ProviderInvalidated
            }
            "account_removed" | "account-removed" => pi::auth::RevocationReason::AccountRemoved,
            _ => pi::auth::RevocationReason::UserRequested,
        }
    }

    match command {
        AccountCommands::List { provider } => {
            let auth = pi::auth::AuthStorage::load(auth_path)?;
            let provider = provider.unwrap_or_else(|| "all".to_string());
            let output = auth.format_accounts_status(&provider);
            println!("{output}");
        }
        AccountCommands::Use { provider, account } => {
            let mut auth = pi::auth::AuthStorage::load(auth_path)?;
            auth.set_active_account(&provider, &account)?;
            auth.save()?;
            println!("Switched to account '{account}' for {provider}");
        }
        AccountCommands::Remove { provider, account } => {
            let mut auth = pi::auth::AuthStorage::load(auth_path)?;
            // Use the existing remove method for the account
            let accounts = auth.list_oauth_accounts(&provider);
            if accounts.iter().any(|a| a.id == account) {
                auth.remove_oauth_account(&provider, &account);
                auth.save()?;
                println!("Removed account '{account}' from {provider}");
            } else {
                bail!("Account '{account}' not found in {provider}");
            }
        }
        AccountCommands::Logout { provider } => {
            let mut auth = pi::auth::AuthStorage::load(auth_path)?;
            if let Some(provider) = provider {
                // Remove specific provider's credentials
                auth.remove(&provider);
                auth.save()?;
                println!("Logged out from {provider}");
            } else {
                // Logout from all providers
                let providers = auth.provider_names();
                for p in providers {
                    auth.remove(&p);
                }
                auth.save()?;
                println!("Logged out from all providers");
            }
        }
        AccountCommands::Status { provider, json } => {
            let auth = pi::auth::AuthStorage::load(auth_path)?;
            if json {
                if let Some(p) = provider {
                    let accounts = auth.list_oauth_accounts(&p);
                    let payload = serde_json::json!({
                        "provider": p,
                        "accounts": accounts,
                        "revocations": auth.revocation_records(),
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    let providers = auth.provider_names();
                    let mut provider_payload = serde_json::Map::new();
                    for p in providers {
                        provider_payload.insert(
                            p.clone(),
                            serde_json::json!({
                                "accounts": auth.list_oauth_accounts(&p),
                                "recovery": auth.recovery_state_summary(&p),
                            }),
                        );
                    }
                    let payload = serde_json::json!({
                        "providers": provider_payload,
                        "revocations": auth.revocation_records(),
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
            } else if let Some(p) = provider {
                let output = auth.format_accounts_status(&p);
                println!("{output}");
            } else {
                // List all providers
                let providers = auth.provider_names();
                if providers.is_empty() {
                    println!("No providers configured.");
                } else {
                    for p in providers {
                        println!("Provider: {p}");
                        let output = auth.format_accounts_status(&p);
                        println!("{output}");
                    }
                }
            }
        }
        AccountCommands::Cleanup { dry_run } => {
            let mut auth = pi::auth::AuthStorage::load(auth_path)?;
            if dry_run {
                let report = auth.cleanup_tokens_preview();
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let report = auth.cleanup_tokens();
                auth.save()?;
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
        }
        AccountCommands::Revoke {
            provider,
            account,
            reason,
        } => {
            let mut auth = pi::auth::AuthStorage::load(auth_path)?;
            let reason = parse_revocation_reason(reason);
            auth.revoke_oauth_account_token(&provider, &account, reason)?;
            auth.save()?;
            println!("Revoked account '{account}' for {provider} ({reason})");
        }
        AccountCommands::Recovery {
            provider,
            account,
            json,
        } => {
            let auth = pi::auth::AuthStorage::load(auth_path)?;
            let mut rows = auth.recovery_state_summary(&provider);
            if let Some(account_id) = account.as_deref() {
                rows.retain(|(id, _)| id == account_id);
            }
            if json {
                let payload = serde_json::json!({
                    "provider": provider,
                    "recovery": rows,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else if rows.is_empty() {
                println!("No recovery state found for provider '{provider}'.");
            } else {
                for (account_id, state) in rows {
                    println!(
                        "{}: {}",
                        account_id,
                        state.as_ref().map_or_else(
                            || "none".to_string(),
                            pi::auth::RecoveryState::description
                        )
                    );
                }
            }
        }
    }

    Ok(())
}

fn handle_doctor(
    cwd: &Path,
    extension_path: Option<&str>,
    format: &str,
    policy_override: Option<&str>,
    fix: bool,
    only: Option<&str>,
) -> Result<()> {
    use pi::doctor::{CheckCategory, DoctorOptions};

    let only_set = if let Some(raw) = only {
        let mut parsed = std::collections::HashSet::new();
        let mut invalid = Vec::new();
        for part in raw.split(',') {
            let name = part.trim();
            if name.is_empty() {
                continue;
            }
            match name.parse::<CheckCategory>() {
                Ok(cat) => {
                    parsed.insert(cat);
                }
                Err(_) => invalid.push(name.to_string()),
            }
        }
        if !invalid.is_empty() {
            bail!(
                "Unknown --only categories: {} (valid: config, dirs, auth, shell, sessions, extensions)",
                invalid.join(", ")
            );
        }
        if parsed.is_empty() {
            bail!(
                "--only must include at least one category (valid: config, dirs, auth, shell, sessions, extensions)"
            );
        }
        Some(parsed)
    } else {
        None
    };

    let opts = DoctorOptions {
        cwd,
        extension_path,
        policy_override,
        fix,
        only: only_set,
    };

    let report = pi::doctor::run_doctor(&opts)?;

    match format {
        "json" => {
            println!("{}", report.to_json()?);
        }
        "markdown" | "md" => {
            print!("{}", report.render_markdown());
        }
        _ => {
            print!("{}", report.render_text());
        }
    }

    // Exit with code 1 if any failures (useful for CI)
    if report.overall == pi::doctor::Severity::Fail {
        std::process::exit(1);
    }

    Ok(())
}

fn print_version() {
    println!(
        "pi {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
    );
}

fn list_models(registry: &ModelRegistry, pattern: Option<&str>) {
    let mut models = registry.get_available();
    if models.is_empty() {
        println!("No models available. Set API keys in environment variables.");
        return;
    }

    if let Some(pattern) = pattern {
        models = filter_models_by_pattern(models, pattern);
        if models.is_empty() {
            println!("No models matching \"{pattern}\"");
            return;
        }
    }

    models.sort_by(|a, b| {
        let provider_cmp = a.model.provider.cmp(&b.model.provider);
        if provider_cmp == std::cmp::Ordering::Equal {
            a.model.id.cmp(&b.model.id)
        } else {
            provider_cmp
        }
    });

    let rows = build_model_rows(&models);
    print_model_table(&rows);
}

fn list_providers() {
    let mut rows: Vec<(&str, &str, String, String, &str)> = PROVIDER_METADATA
        .iter()
        .map(|meta| {
            let display = meta.canonical_id;
            let aliases = if meta.aliases.is_empty() {
                String::new()
            } else {
                meta.aliases.join(", ")
            };
            let env_keys = meta.auth_env_keys.join(", ");
            let api = meta.routing_defaults.map_or("-", |defaults| defaults.api);
            (meta.canonical_id, display, aliases, env_keys, api)
        })
        .collect();
    rows.sort_by_key(|(id, _, _, _, _)| *id);

    let id_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(8);
    let name_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(4);
    let alias_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(7);
    let env_w = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(8);
    let api_w = rows.iter().map(|r| r.4.len()).max().unwrap_or(0).max(3);

    println!(
        "{:<id_w$}  {:<name_w$}  {:<alias_w$}  {:<env_w$}  {:<api_w$}",
        "provider", "name", "aliases", "auth env", "api",
    );
    println!(
        "{:<id_w$}  {:<name_w$}  {:<alias_w$}  {:<env_w$}  {:<api_w$}",
        "-".repeat(id_w),
        "-".repeat(name_w),
        "-".repeat(alias_w),
        "-".repeat(env_w),
        "-".repeat(api_w),
    );
    for (id, name, aliases, env_keys, api) in &rows {
        println!(
            "{id:<id_w$}  {name:<name_w$}  {aliases:<alias_w$}  {env_keys:<env_w$}  {api:<api_w$}"
        );
    }
    println!("\n{} providers available.", rows.len());
}

fn filter_models_by_pattern(models: Vec<ModelEntry>, pattern: &str) -> Vec<ModelEntry> {
    models
        .into_iter()
        .filter(|entry| {
            fuzzy_match(
                pattern,
                &format!("{} {}", entry.model.provider, entry.model.id),
            )
        })
        .collect()
}

fn build_model_rows(
    models: &[ModelEntry],
) -> Vec<(String, String, String, String, String, String)> {
    models
        .iter()
        .map(|entry| {
            let provider = entry.model.provider.clone();
            let model = entry.model.id.clone();
            let context = format_token_count(entry.model.context_window);
            let max_out = format_token_count(entry.model.max_tokens);
            let thinking = if entry.model.reasoning { "yes" } else { "no" }.to_string();
            let images = if entry.model.input.contains(&InputType::Image) {
                "yes"
            } else {
                "no"
            }
            .to_string();
            (provider, model, context, max_out, thinking, images)
        })
        .collect()
}

fn print_model_table(rows: &[(String, String, String, String, String, String)]) {
    let headers = (
        "provider", "model", "context", "max-out", "thinking", "images",
    );

    let provider_w = rows
        .iter()
        .map(|r| r.0.len())
        .max()
        .unwrap_or(0)
        .max(headers.0.len());
    let model_w = rows
        .iter()
        .map(|r| r.1.len())
        .max()
        .unwrap_or(0)
        .max(headers.1.len());
    let context_w = rows
        .iter()
        .map(|r| r.2.len())
        .max()
        .unwrap_or(0)
        .max(headers.2.len());
    let max_out_w = rows
        .iter()
        .map(|r| r.3.len())
        .max()
        .unwrap_or(0)
        .max(headers.3.len());
    let thinking_w = rows
        .iter()
        .map(|r| r.4.len())
        .max()
        .unwrap_or(0)
        .max(headers.4.len());
    let images_w = rows
        .iter()
        .map(|r| r.5.len())
        .max()
        .unwrap_or(0)
        .max(headers.5.len());

    let (provider, model, context, max_out, thinking, images) = headers;
    println!(
        "{provider:<provider_w$}  {model:<model_w$}  {context:<context_w$}  {max_out:<max_out_w$}  {thinking:<thinking_w$}  {images:<images_w$}"
    );

    for (provider, model, context, max_out, thinking, images) in rows {
        println!(
            "{provider:<provider_w$}  {model:<model_w$}  {context:<context_w$}  {max_out:<max_out_w$}  {thinking:<thinking_w$}  {images:<images_w$}"
        );
    }
}

async fn run_rpc_mode(
    session: AgentSession,
    resources: ResourceLoader,
    config: Config,
    available_models: Vec<ModelEntry>,
    scoped_models: Vec<pi::rpc::RpcScopedModel>,
    auth: AuthStorage,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    use futures::FutureExt;

    let (abort_handle, abort_signal) = AbortHandle::new();
    let abort_listener = abort_handle.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        abort_listener.abort();
    }) {
        eprintln!("Warning: Failed to install Ctrl+C handler for RPC mode: {err}");
    }
    let rpc_task = pi::rpc::run_stdio(
        session,
        pi::rpc::RpcOptions {
            config,
            resources,
            available_models,
            scoped_models,
            auth,
            runtime_handle,
        },
    )
    .fuse();

    let signal_task = abort_signal.wait().fuse();

    futures::pin_mut!(rpc_task, signal_task);

    match futures::future::select(rpc_task, signal_task).await {
        futures::future::Either::Left((result, _)) => match result {
            Ok(()) => Ok(()),
            Err(err) => Err(anyhow::Error::new(err)),
        },
        futures::future::Either::Right(((), _)) => {
            // Signal received, return Ok to trigger main_impl's shutdown flush
            Ok(())
        }
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_print_mode(
    session: &mut AgentSession,
    mode: &str,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    resources: &ResourceLoader,
    runtime_handle: RuntimeHandle,
    config: &Config,
) -> Result<()> {
    // Fail closed: non-interactive modes cannot handle approval prompts.
    // Print mode is always non-interactive and should never fall back to TTY prompts.
    // Extension capability requests are handled by the extension UI mechanism,
    // which fails closed when no UI sender is configured.

    if mode != "text" && mode != "json" {
        bail!("Unknown mode: {mode}");
    }

    let extension_ui_blocked =
        pi::surface::install_non_interactive_extension_ui_bridge(session, &runtime_handle);

    if mode == "json" {
        let cx = pi::agent_cx::AgentCx::for_request();
        let session = session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("{}", serde_json::to_string(&session.header)?);
    }
    if initial.is_none() && messages.is_empty() {
        if mode == "json" {
            io::stdout().flush()?;
            return Ok(());
        }
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    let mut last_message: Option<AssistantMessage> = None;
    let extensions = session.extensions.as_ref().map(|r| r.manager().clone());
    let emit_json_events = mode == "json";
    let runtime_for_events = runtime_handle.clone();
    let make_event_handler = move || {
        let extensions = extensions.clone();
        let runtime_for_events = runtime_for_events.clone();
        let coalescer = extensions
            .as_ref()
            .map(|m| pi::extensions::EventCoalescer::new(m.clone()));
        move |event: AgentEvent| {
            if emit_json_events {
                if let Ok(serialized) = serde_json::to_string(&event) {
                    println!("{serialized}");
                }
            }
            // Route non-lifecycle events through the coalescer for
            // batched/coalesced dispatch with lazy serialization.
            if let Some(coal) = &coalescer {
                coal.dispatch_agent_event_lazy(&event, &runtime_for_events);
            }
        }
    };
    let (abort_handle, abort_signal) = AbortHandle::new();
    let abort_listener = abort_handle.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        abort_listener.abort();
    }) {
        eprintln!("Warning: Failed to install Ctrl+C handler: {err}");
    }

    let mut initial = initial;
    if let Some(ref mut initial) = initial {
        initial.text = resources.expand_input(&initial.text);
    }

    let messages = messages
        .into_iter()
        .map(|message| resources.expand_input(&message))
        .collect::<Vec<_>>();

    let retry_enabled = config.retry_enabled();
    let max_retries = config.retry_max_retries();
    let is_json = mode == "json";

    if let Some(initial) = initial {
        let content = pi::app::build_initial_content(&initial);
        last_message = Some(
            run_print_prompt_with_retry(
                session,
                config,
                &abort_signal,
                &make_event_handler,
                retry_enabled,
                max_retries,
                is_json,
                PromptInput::Content(content),
            )
            .await?,
        );
        pi::surface::check_non_interactive_extension_ui_blocked(&extension_ui_blocked)?;
    }

    for message in messages {
        last_message = Some(
            run_print_prompt_with_retry(
                session,
                config,
                &abort_signal,
                &make_event_handler,
                retry_enabled,
                max_retries,
                is_json,
                PromptInput::Text(message),
            )
            .await?,
        );
        pi::surface::check_non_interactive_extension_ui_blocked(&extension_ui_blocked)?;
    }

    let Some(last_message) = last_message else {
        if mode == "json" {
            io::stdout().flush()?;
            return Ok(());
        }
        bail!("No messages were sent");
    };

    if mode == "text" {
        if matches!(
            last_message.stop_reason,
            StopReason::Error | StopReason::Aborted
        ) {
            let message = last_message
                .error_message
                .unwrap_or_else(|| "Request error".to_string());
            bail!(message);
        }
        // When stdout is a terminal, render markdown with formatting.
        // When piped, emit plain text via output_final_text to avoid escape codes.
        if std::io::IsTerminal::is_terminal(&io::stdout()) {
            let mut markdown = String::new();
            for block in &last_message.content {
                if let ContentBlock::Text(text) = block {
                    markdown.push_str(&text.text);
                    if !markdown.ends_with('\n') {
                        markdown.push('\n');
                    }
                }
            }

            if !markdown.is_empty() {
                let console = PiConsole::new();
                console.render_markdown(&markdown);
            }
        } else {
            pi::app::output_final_text(&last_message);
        }
    }

    io::stdout().flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_selected_surface_route(
    route_kind: pi::surface::CliRouteKind,
    mut agent_session: AgentSession,
    selection: &pi::app::ModelSelection,
    available_models: Vec<ModelEntry>,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    resources: ResourceLoader,
    config: &Config,
    auth: AuthStorage,
    runtime_handle: RuntimeHandle,
    resource_cli: ResourceCliOptions,
    cwd: PathBuf,
    save_enabled: bool,
    mode: &str,
) -> Result<()> {
    match route_kind {
        pi::surface::CliRouteKind::Rpc => {
            let rpc_scoped_models = selection
                .scoped_models
                .iter()
                .map(|sm| pi::rpc::RpcScopedModel {
                    model: sm.model.clone(),
                    thinking_level: sm.thinking_level,
                })
                .collect::<Vec<_>>();
            Box::pin(run_rpc_mode(
                agent_session,
                resources,
                config.clone(),
                available_models,
                rpc_scoped_models,
                auth,
                runtime_handle,
            ))
            .await
        }
        pi::surface::CliRouteKind::Interactive => {
            let model_scope = selection
                .scoped_models
                .iter()
                .map(|sm| sm.model.clone())
                .collect::<Vec<_>>();

            run_interactive_mode(
                agent_session,
                initial,
                messages,
                config.clone(),
                selection.model_entry.clone(),
                model_scope,
                available_models,
                save_enabled,
                resources,
                resource_cli,
                cwd,
                runtime_handle,
            )
            .await
        }
        pi::surface::CliRouteKind::Print
        | pi::surface::CliRouteKind::Json
        | pi::surface::CliRouteKind::Text => {
            let result = run_print_mode(
                &mut agent_session,
                mode,
                initial,
                messages,
                &resources,
                runtime_handle,
                config,
            )
            .await;
            // Explicitly shut down extension runtimes before the session drops.
            // Without this, ExtensionRegion::drop() runs synchronously and cannot
            // coordinate with the QuickJS runtime thread, causing a GC assertion
            // failure (non-empty gc_obj_list) when 2+ JS extensions are loaded.
            if let Some(ref ext) = agent_session.extensions {
                ext.shutdown().await;
            }
            result
        }
        pi::surface::CliRouteKind::Subcommand | pi::surface::CliRouteKind::FastOffline => {
            unreachable!("subcommands and fast-offline routes do not enter run()")
        }
    }
}

/// Discriminated prompt input for retry helper.
enum PromptInput {
    Text(String),
    Content(Vec<ContentBlock>),
}

/// Compute retry delay with exponential backoff (mirrors RPC mode logic).
fn print_mode_retry_delay_ms(config: &Config, attempt: u32) -> u32 {
    let base = u64::from(config.retry_base_delay_ms());
    let max = u64::from(config.retry_max_delay_ms());
    let shift = attempt.saturating_sub(1);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = base.saturating_mul(multiplier).min(max);
    u32::try_from(delay).unwrap_or(u32::MAX)
}

/// Emit a JSON-serialized [`AgentEvent`] to stdout (for JSON print mode).
fn emit_json_event(event: &AgentEvent) {
    if let Ok(serialized) = serde_json::to_string(event) {
        println!("{serialized}");
    }
}

/// Check whether a prompt result is a retryable error.
fn is_retryable_prompt_result(msg: &AssistantMessage) -> bool {
    if !matches!(msg.stop_reason, StopReason::Error) {
        return false;
    }
    let err_msg = msg.error_message.as_deref().unwrap_or("Request error");
    pi::error::is_retryable_error(err_msg, Some(msg.usage.input), None)
}

/// Execute a single prompt with automatic retry and `AutoRetryStart`/`AutoRetryEnd`
/// event emission. Mirrors the retry behaviour in RPC mode (`src/rpc.rs`).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_print_prompt_with_retry<H, EH>(
    session: &mut AgentSession,
    config: &Config,
    abort_signal: &pi::agent::AbortSignal,
    make_event_handler: &H,
    retry_enabled: bool,
    max_retries: u32,
    is_json: bool,
    input: PromptInput,
) -> Result<AssistantMessage>
where
    H: Fn() -> EH + Sync,
    EH: Fn(AgentEvent) + Send + Sync + 'static,
{
    // First attempt.
    let first_result = match &input {
        PromptInput::Text(text) => {
            session
                .run_text_with_abort(
                    text.clone(),
                    Some(abort_signal.clone()),
                    make_event_handler(),
                )
                .await
        }
        PromptInput::Content(content) => {
            session
                .run_with_content_with_abort(
                    content.clone(),
                    Some(abort_signal.clone()),
                    make_event_handler(),
                )
                .await
        }
    };

    // Fast path: no retry needed.
    if !retry_enabled {
        return first_result.map_err(anyhow::Error::new);
    }

    let mut retry_count: u32 = 0;
    let mut current_result = first_result;

    loop {
        match current_result {
            Ok(msg) if msg.stop_reason == StopReason::Aborted => {
                if retry_count > 0 && is_json {
                    emit_json_event(&AgentEvent::AutoRetryEnd {
                        success: false,
                        attempt: retry_count,
                        final_error: Some("Aborted".to_string()),
                    });
                }
                return Ok(msg);
            }
            Ok(msg) if is_retryable_prompt_result(&msg) && retry_count < max_retries => {
                let err_msg = msg
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Request error".to_string());

                retry_count += 1;
                let delay_ms = print_mode_retry_delay_ms(config, retry_count);
                if is_json {
                    emit_json_event(&AgentEvent::AutoRetryStart {
                        attempt: retry_count,
                        max_attempts: max_retries,
                        delay_ms: u64::from(delay_ms),
                        error_message: err_msg,
                    });
                }

                asupersync::time::sleep(
                    asupersync::time::wall_now(),
                    Duration::from_millis(u64::from(delay_ms)),
                )
                .await;

                // Re-send the same prompt (matches RPC retry behaviour).
                current_result = match &input {
                    PromptInput::Text(text) => {
                        session
                            .run_text_with_abort(
                                text.clone(),
                                Some(abort_signal.clone()),
                                make_event_handler(),
                            )
                            .await
                    }
                    PromptInput::Content(content) => {
                        session
                            .run_with_content_with_abort(
                                content.clone(),
                                Some(abort_signal.clone()),
                                make_event_handler(),
                            )
                            .await
                    }
                };
            }
            Ok(msg) => {
                // Success or non-retryable error or max retries reached.
                let success = !matches!(msg.stop_reason, StopReason::Error);
                if retry_count > 0 && is_json {
                    emit_json_event(&AgentEvent::AutoRetryEnd {
                        success,
                        attempt: retry_count,
                        final_error: if success {
                            None
                        } else {
                            msg.error_message.clone()
                        },
                    });
                }
                return Ok(msg);
            }
            Err(err) => {
                let err_str = err.to_string();
                if retry_count < max_retries && pi::error::is_retryable_error(&err_str, None, None)
                {
                    retry_count += 1;
                    let delay_ms = print_mode_retry_delay_ms(config, retry_count);
                    if is_json {
                        emit_json_event(&AgentEvent::AutoRetryStart {
                            attempt: retry_count,
                            max_attempts: max_retries,
                            delay_ms: u64::from(delay_ms),
                            error_message: err_str,
                        });
                    }

                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(u64::from(delay_ms)),
                    )
                    .await;

                    current_result = match &input {
                        PromptInput::Text(text) => {
                            session
                                .run_text_with_abort(
                                    text.clone(),
                                    Some(abort_signal.clone()),
                                    make_event_handler(),
                                )
                                .await
                        }
                        PromptInput::Content(content) => {
                            session
                                .run_with_content_with_abort(
                                    content.clone(),
                                    Some(abort_signal.clone()),
                                    make_event_handler(),
                                )
                                .await
                        }
                    };
                } else {
                    if retry_count > 0 && is_json {
                        emit_json_event(&AgentEvent::AutoRetryEnd {
                            success: false,
                            attempt: retry_count,
                            final_error: Some(err_str),
                        });
                    }
                    return Err(anyhow::Error::new(err));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive_mode(
    session: AgentSession,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    config: Config,
    model_entry: ModelEntry,
    model_scope: Vec<ModelEntry>,
    available_models: Vec<ModelEntry>,
    save_enabled: bool,
    resources: ResourceLoader,
    resource_cli: ResourceCliOptions,
    cwd: PathBuf,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    let mut pending = Vec::new();
    if let Some(initial) = initial {
        pending.push(pi::interactive::PendingInput::Content(
            pi::app::build_initial_content(&initial),
        ));
    }
    for message in messages {
        pending.push(pi::interactive::PendingInput::Text(message));
    }

    let AgentSession {
        agent,
        session,
        extensions: region,
        ..
    } = session;
    // Extract manager for the interactive loop; the region stays alive to
    // handle shutdown when this scope exits.
    let extensions = region.as_ref().map(|r| r.manager().clone());
    let interactive_result = pi::interactive::run_interactive(
        agent,
        session,
        config,
        model_entry,
        model_scope,
        available_models,
        pending,
        save_enabled,
        resources,
        resource_cli,
        extensions,
        cwd,
        runtime_handle,
    )
    .await;
    // Explicitly shut down extension runtimes so the QuickJS GC can
    // collect all objects before JS_FreeRuntime asserts an empty gc_obj_list.
    // Must run even on error — otherwise ExtensionRegion::drop() runs
    // synchronously and the GC assertion fires.
    if let Some(ref region) = region {
        region.shutdown().await;
    }
    interactive_result?;
    Ok(())
}

type InitialMessage = pi::app::InitialMessage;

fn read_piped_stdin() -> Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut data = String::new();
    io::stdin().read_to_string(&mut data)?;
    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(data))
    }
}

fn format_token_count(count: u32) -> String {
    if count >= 1_000_000 {
        let millions = f64::from(count) / 1_000_000.0;
        if millions.fract() == 0.0 {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        }
    } else if count >= 1_000 {
        let thousands = f64::from(count) / 1_000.0;
        if thousands.fract() == 0.0 {
            format!("{thousands:.0}K")
        } else {
            format!("{thousands:.1}K")
        }
    } else {
        count.to_string()
    }
}

fn fuzzy_match(pattern: &str, value: &str) -> bool {
    let needle_str = pattern.to_lowercase();
    let haystack_str = value.to_lowercase();
    let mut needle = needle_str.chars().filter(|c| !c.is_whitespace());
    let mut haystack = haystack_str.chars();
    for ch in needle.by_ref() {
        if !haystack.by_ref().any(|h| h == ch) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use clap::Parser;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn exit_code_classifier_marks_usage_errors() {
        let usage_err = anyhow!("Unknown --only categories: nope");
        assert_eq!(exit_code_for_error(&usage_err), EXIT_CODE_USAGE);

        let validation_err = anyhow::Error::new(pi::error::Error::validation("bad input"));
        assert_eq!(exit_code_for_error(&validation_err), EXIT_CODE_USAGE);
    }

    #[test]
    fn exit_code_classifier_defaults_to_general_failure() {
        let runtime_err = anyhow::Error::new(pi::error::Error::auth("missing key"));
        assert_eq!(exit_code_for_error(&runtime_err), EXIT_CODE_FAILURE);
    }

    #[test]
    fn parse_cli_args_extracts_extension_flags() {
        let parsed = parse_cli_args(vec![
            "pi".to_string(),
            "--model".to_string(),
            "gpt-4o".to_string(),
            "--plan".to_string(),
            "ship-it".to_string(),
            "--dry-run".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse args")
        .expect("parsed cli payload");

        assert_eq!(parsed.0.model.as_deref(), Some("gpt-4o"));
        assert!(parsed.0.print);
        assert_eq!(parsed.1.len(), 2);
        assert_eq!(parsed.1[0].name, "plan");
        assert_eq!(parsed.1[0].value.as_deref(), Some("ship-it"));
        assert_eq!(parsed.1[1].name, "dry-run");
        assert!(parsed.1[1].value.is_none());
    }

    #[test]
    fn parse_cli_args_keeps_subcommand_validation() {
        let result = parse_cli_args(vec![
            "pi".to_string(),
            "install".to_string(),
            "--bogus".to_string(),
            "pkg".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn collect_session_jsonl_files_walks_nested_session_dirs() {
        let temp = TempDir::new().expect("tempdir");
        let nested = temp.path().join("--repo--").join("archived");
        std::fs::create_dir_all(&nested).expect("create nested dirs");

        let root_file = temp.path().join("top-level.jsonl");
        let nested_file = nested.join("nested.jsonl");
        let ignored_file = nested.join("notes.txt");

        std::fs::write(&root_file, "{}").expect("write root session");
        std::fs::write(&nested_file, "{}").expect("write nested session");
        std::fs::write(&ignored_file, "ignore").expect("write ignored file");

        let files = collect_session_jsonl_files(temp.path()).expect("collect jsonl files");
        assert_eq!(files, vec![nested_file, root_file]);
    }

    #[test]
    fn coerce_extension_flag_bool_defaults_to_true_without_value() {
        let flag = cli::ExtensionCliFlag {
            name: "dry-run".to_string(),
            value: None,
        };
        let value = coerce_extension_flag_value(&flag, "bool").expect("coerce bool");
        assert_eq!(value, Value::Bool(true));
    }

    #[test]
    fn coerce_extension_flag_rejects_invalid_bool_text() {
        let flag = cli::ExtensionCliFlag {
            name: "dry-run".to_string(),
            value: Some("maybe".to_string()),
        };
        let err = coerce_extension_flag_value(&flag, "bool").expect_err("invalid bool should fail");
        assert!(err.to_string().contains("Invalid boolean value"));
    }

    #[test]
    fn provider_choice_from_token_numbered_choices() {
        let choice = provider_choice_from_token("1").expect("provider 1");
        assert_eq!(choice.provider, "openai-codex");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("2").expect("provider 2");
        assert_eq!(choice.provider, "openai");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("3").expect("provider 3");
        assert_eq!(choice.provider, "anthropic");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("4").expect("provider 4");
        assert_eq!(choice.provider, "anthropic");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("5").expect("provider 5");
        assert_eq!(choice.provider, "kimi-for-coding");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthDeviceFlow);

        let choice = provider_choice_from_token("6").expect("provider 6");
        assert_eq!(choice.provider, "google-gemini-cli");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("7").expect("provider 7");
        assert_eq!(choice.provider, "google");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("8").expect("provider 8");
        assert_eq!(choice.provider, "google-antigravity");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("9").expect("provider 9");
        assert_eq!(choice.provider, "azure-openai");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("10").expect("provider 10");
        assert_eq!(choice.provider, "openrouter");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);
        // Out of range
        assert!(provider_choice_from_token("0").is_none());
        assert!(provider_choice_from_token("11").is_none());
    }

    #[test]
    fn provider_choice_from_token_common_nicknames() {
        assert_eq!(
            provider_choice_from_token("claude").unwrap().provider,
            "anthropic"
        );
        assert_eq!(
            provider_choice_from_token("gpt").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(
            provider_choice_from_token("chatgpt").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(
            provider_choice_from_token("gemini").unwrap().provider,
            "google"
        );
        assert_eq!(
            provider_choice_from_token("kimi").unwrap().provider,
            "kimi-for-coding"
        );
    }

    #[test]
    fn provider_choice_from_token_canonical_ids() {
        assert_eq!(
            provider_choice_from_token("anthropic").unwrap().provider,
            "anthropic"
        );
        assert_eq!(
            provider_choice_from_token("openai").unwrap().provider,
            "openai"
        );
        assert_eq!(
            provider_choice_from_token("openai-codex").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(provider_choice_from_token("groq").unwrap().provider, "groq");
        assert_eq!(
            provider_choice_from_token("openrouter").unwrap().provider,
            "openrouter"
        );
        assert_eq!(
            provider_choice_from_token("mistral").unwrap().provider,
            "mistral"
        );
    }

    #[test]
    fn provider_choice_from_token_case_insensitive() {
        assert_eq!(
            provider_choice_from_token("ANTHROPIC").unwrap().provider,
            "anthropic"
        );
        assert_eq!(provider_choice_from_token("Groq").unwrap().provider, "groq");
        assert_eq!(
            provider_choice_from_token("OpenRouter").unwrap().provider,
            "openrouter"
        );
    }

    #[test]
    fn provider_choice_from_token_metadata_fallback() {
        // Providers not in the top-10 list but in provider_metadata registry
        assert_eq!(
            provider_choice_from_token("deepseek").unwrap().provider,
            "deepseek"
        );
        assert_eq!(
            provider_choice_from_token("cerebras").unwrap().provider,
            "cerebras"
        );
        assert_eq!(
            provider_choice_from_token("cohere").unwrap().provider,
            "cohere"
        );
        assert_eq!(
            provider_choice_from_token("perplexity").unwrap().provider,
            "perplexity"
        );
        // Aliases resolve through metadata
        assert_eq!(
            provider_choice_from_token("open-router").unwrap().provider,
            "openrouter"
        );
        assert_eq!(
            provider_choice_from_token("dashscope").unwrap().provider,
            "alibaba"
        );
    }

    #[test]
    fn provider_choice_from_token_honors_method_preference() {
        let provider = provider_choice_from_token("anthropic oauth").expect("anthropic oauth");
        assert_eq!(provider.provider, "anthropic");
        assert_eq!(provider.kind, SetupCredentialKind::OAuthPkce);

        let provider = provider_choice_from_token("anthropic key").expect("anthropic key");
        assert_eq!(provider.provider, "anthropic");
        assert_eq!(provider.kind, SetupCredentialKind::ApiKey);
    }

    #[test]
    fn provider_choice_from_token_whitespace_handling() {
        assert_eq!(
            provider_choice_from_token("  groq  ").unwrap().provider,
            "groq"
        );
        assert_eq!(
            provider_choice_from_token(" 1 ").unwrap().provider,
            "openai-codex"
        );
    }

    #[test]
    fn provider_choice_from_token_unknown_returns_none() {
        assert!(provider_choice_from_token("nonexistent-provider-xyz").is_none());
        assert!(provider_choice_from_token("").is_none());
    }

    #[test]
    fn config_ui_app_empty_packages_shows_empty_message() {
        let result_slot = Arc::new(StdMutex::new(None));
        let app = ConfigUiApp::new(
            Vec::new(),
            "provider=(default)  model=(default)  thinking=(default)".to_string(),
            result_slot,
        );

        let view = app.view();
        assert!(
            view.contains("Pi Config UI"),
            "missing config ui header:\n{view}"
        );
        assert!(
            view.contains("No package resources discovered. Press Enter to exit."),
            "missing empty packages hint:\n{view}"
        );
    }

    #[test]
    fn config_ui_app_toggle_selected_updates_resource_state() {
        let result_slot = Arc::new(StdMutex::new(None));
        let mut app = ConfigUiApp::new(
            vec![ConfigPackageState {
                scope: SettingsScope::Project,
                source: "local:demo".to_string(),
                resources: vec![
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/a.js".to_string(),
                        enabled: true,
                    },
                    ConfigResourceState {
                        kind: ConfigResourceKind::Skills,
                        path: "skills/demo/SKILL.md".to_string(),
                        enabled: false,
                    },
                ],
            }],
            "provider=(default)  model=(default)  thinking=(default)".to_string(),
            result_slot,
        );

        assert!(
            app.packages[0].resources[0].enabled,
            "first resource should start enabled"
        );
        app.toggle_selected();
        assert!(
            !app.packages[0].resources[0].enabled,
            "toggling selected resource should flip enabled flag"
        );

        app.move_selection(1);
        app.toggle_selected();
        assert!(
            app.packages[0].resources[1].enabled,
            "second resource should toggle on after moving selection"
        );
    }

    #[test]
    fn format_settings_summary_uses_effective_config_values() {
        let config = Config {
            default_provider: Some("openai".to_string()),
            default_model: Some("gpt-4.1".to_string()),
            default_thinking_level: Some("high".to_string()),
            ..Config::default()
        };

        assert_eq!(
            format_settings_summary(&config),
            "provider=openai  model=gpt-4.1  thinking=high"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn persist_package_toggles_writes_filters_per_scope() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path().join("repo");
        let global_dir = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&global_dir).expect("create global dir");
        std::fs::create_dir_all(cwd.join(".pi")).expect("create project .pi");

        std::fs::write(
            global_dir.join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "packages": ["npm:foo"]
            }))
            .expect("serialize global settings"),
        )
        .expect("write global settings");

        std::fs::write(
            cwd.join(".pi").join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "packages": [
                    {
                        "source": "npm:bar",
                        "local": true,
                        "kind": "npm"
                    }
                ]
            }))
            .expect("serialize project settings"),
        )
        .expect("write project settings");

        let packages = vec![
            ConfigPackageState {
                scope: SettingsScope::Global,
                source: "npm:foo".to_string(),
                resources: vec![
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/a.js".to_string(),
                        enabled: true,
                    },
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/b.js".to_string(),
                        enabled: false,
                    },
                ],
            },
            ConfigPackageState {
                scope: SettingsScope::Project,
                source: "npm:bar".to_string(),
                resources: vec![ConfigResourceState {
                    kind: ConfigResourceKind::Skills,
                    path: "skills/demo/SKILL.md".to_string(),
                    enabled: true,
                }],
            },
        ];

        persist_package_toggles_with_roots(&cwd, &global_dir, &packages)
            .expect("persist package toggles");

        let global_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(global_dir.join("settings.json")).expect("read global"),
        )
        .expect("parse global json");
        let global_pkg = global_value["packages"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_object)
            .expect("global package object");
        assert_eq!(
            global_pkg
                .get("source")
                .and_then(serde_json::Value::as_str)
                .expect("source"),
            "npm:foo"
        );
        assert_eq!(
            global_pkg
                .get("extensions")
                .and_then(serde_json::Value::as_array)
                .expect("extensions")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["extensions/a.js"]
        );

        let project_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(cwd.join(".pi").join("settings.json")).expect("read project"),
        )
        .expect("parse project json");
        let project_pkg = project_value["packages"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_object)
            .expect("project package object");
        assert_eq!(
            project_pkg
                .get("source")
                .and_then(serde_json::Value::as_str)
                .expect("source"),
            "npm:bar"
        );
        assert_eq!(
            project_pkg
                .get("skills")
                .and_then(serde_json::Value::as_array)
                .expect("skills")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["skills/demo/SKILL.md"]
        );
        assert!(
            project_pkg
                .get("local")
                .and_then(serde_json::Value::as_bool)
                .expect("local")
        );
    }

    // ================================================================
    // Retry helper tests
    // ================================================================

    #[test]
    fn print_mode_retry_delay_first_attempt_is_base() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(3),
                reviewer_model: None,
                token_budget: None,
                base_delay_ms: Some(2000),
                max_delay_ms: Some(60_000),
            }),
            ..Config::default()
        };
        assert_eq!(print_mode_retry_delay_ms(&config, 1), 2000);
    }

    #[test]
    fn cli_retry_override_enables_retry_when_missing() {
        let mut config = Config::default();
        let mut cli = cli::Cli::parse_from(["pi"]);
        cli.retry = true;

        apply_cli_retry_override(&mut config, &cli);
        assert!(config.retry_enabled());
        assert_eq!(config.retry_max_retries(), 3);
    }

    #[test]
    fn cli_retry_override_preserves_existing_retry_settings() {
        let mut config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(false),
                max_retries: Some(5),
                reviewer_model: Some("reviewer-x".to_string()),
                token_budget: Some(42_000),
                base_delay_ms: Some(250),
                max_delay_ms: Some(4_000),
            }),
            ..Config::default()
        };
        let mut cli = cli::Cli::parse_from(["pi"]);
        cli.retry = true;

        apply_cli_retry_override(&mut config, &cli);

        assert!(config.retry_enabled());
        assert_eq!(config.retry_max_retries(), 5);
        assert_eq!(config.retry_base_delay_ms(), 250);
        assert_eq!(config.retry_max_delay_ms(), 4_000);
        assert_eq!(config.retry_reviewer_model().as_deref(), Some("reviewer-x"));
        assert_eq!(config.retry_token_budget(), Some(42_000));
    }

    #[test]
    fn print_mode_retry_delay_doubles_each_attempt() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(5),
                reviewer_model: None,
                token_budget: None,
                base_delay_ms: Some(1000),
                max_delay_ms: Some(60_000),
            }),
            ..Config::default()
        };
        assert_eq!(print_mode_retry_delay_ms(&config, 2), 2000);
        assert_eq!(print_mode_retry_delay_ms(&config, 3), 4000);
    }

    #[test]
    fn print_mode_retry_delay_capped_at_max() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(10),
                reviewer_model: None,
                token_budget: None,
                base_delay_ms: Some(2000),
                max_delay_ms: Some(10_000),
            }),
            ..Config::default()
        };
        let delay = print_mode_retry_delay_ms(&config, 5);
        assert!(delay <= 10_000, "delay {delay} should be capped at 10000");
    }

    #[test]
    fn is_retryable_prompt_result_identifies_retryable_errors() {
        use pi::model::{AssistantMessage, Usage};

        let retryable = AssistantMessage {
            content: vec![],
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("429 rate limit exceeded".to_string()),
            timestamp: 0,
        };
        assert!(is_retryable_prompt_result(&retryable));

        let not_retryable = AssistantMessage {
            error_message: Some("invalid api key".to_string()),
            ..retryable.clone()
        };
        assert!(!is_retryable_prompt_result(&not_retryable));

        let success = AssistantMessage {
            stop_reason: StopReason::Stop,
            error_message: None,
            ..retryable
        };
        assert!(!is_retryable_prompt_result(&success));
    }

    #[test]
    fn emit_json_event_serializes_retry_events() {
        let start = AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "rate limited".to_string(),
        };
        let json = serde_json::to_value(&start).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 1);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 2000);

        let end = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        };
        let json = serde_json::to_value(&end).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert!(json["success"].as_bool().unwrap());
    }
}
