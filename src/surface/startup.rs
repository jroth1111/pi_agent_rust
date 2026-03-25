//! Startup recovery and error handling for CLI surfaces.
//!
//! Contains the startup recovery enums and functions that orchestrate
//! first-time setup, auth error recovery, and model selection retry logic.
//! Lives in the surface layer because it is purely startup I/O coordination —
//! no inference, session state, or business-rule ownership.

use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use asupersync::sync::Mutex;

use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::app::{ModelSelection, ScopedModel, StartupError};
use crate::auth::AuthStorage;
use crate::cli;
use crate::compaction::ResolvedCompactionSettings;
use crate::config::Config;
use crate::models::ModelRegistry;
use crate::package_manager::PackageManager;
use crate::providers;
use crate::resources::{ResourceCliOptions, ResourceLoader};
use crate::session::Session;
use crate::surface::auth_setup::run_first_time_setup;
use crate::surface::extension_runtime::{
    SurfaceExtensionPrewarmHandle, activate_extensions_for_session,
};
use crate::tools::ToolRegistry;

pub struct StartupResources {
    pub config: Config,
    pub resources: ResourceLoader,
    pub auth: AuthStorage,
    pub resource_cli: ResourceCliOptions,
}

fn apply_cli_retry_override(config: &mut Config, cli: &cli::Cli) {
    if !cli.retry {
        return;
    }

    let mut retry = config.retry.clone().unwrap_or_default();
    retry.enabled = Some(true);
    config.retry = Some(retry);
}

pub async fn load_surface_startup_resources(
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
        return Err(crate::error::Error::validation(format!(
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

pub async fn refresh_surface_startup_auth(auth: &mut AuthStorage) -> Result<()> {
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

/// Outcome of a surface startup error handling attempt.
pub enum SurfaceStartupRecovery {
    /// Auth setup succeeded — retry the startup flow.
    RetryAfterSetup,
    /// Setup wizard declined or not applicable — exit quietly.
    ExitQuietly,
    /// No recovery action taken.
    None,
}

/// Action to take after attempting to recover from a model-selection startup error.
pub enum SurfaceStartupLoopAction {
    /// Retry model selection (e.g., after successful auth setup).
    ContinueSelection,
    /// Exit quietly without printing an error.
    ExitQuietly,
    /// Propagate the original error to the caller.
    Propagate,
}

/// Classification of what interactive capability a non-interactive startup requires.
pub enum NonInteractiveStartupRequirement<'a> {
    /// No special requirement.
    None,
    /// Requires a TTY for the named operation.
    Tty(&'a str),
    /// Requires an OAuth flow for the named provider.
    OAuth(&'a str),
}

pub struct SurfaceSelectionResolution {
    pub selection: ModelSelection,
    pub resolved_key: Option<String>,
}

pub struct PreparedSurfaceInputs {
    pub global_dir: PathBuf,
    pub models_path: PathBuf,
    pub model_registry: ModelRegistry,
    pub initial: Option<crate::app::InitialMessage>,
    pub messages: Vec<String>,
    pub surface_bootstrap: crate::surface::CliSurfaceBootstrap,
    pub mode: String,
    pub non_interactive_guard: Option<crate::surface::NonInteractiveGuard>,
    pub scoped_patterns: Vec<String>,
}

pub enum SurfaceSelectionOutcome {
    Selected(SurfaceSelectionResolution),
    ExitQuietly,
}

/// Load the model registry, printing a warning on error.
pub fn load_model_registry_with_warning(auth: &AuthStorage, models_path: &Path) -> ModelRegistry {
    let model_registry = ModelRegistry::load(auth, Some(models_path.to_path_buf()));
    if let Some(error) = model_registry.error() {
        eprintln!("Warning: models.json error: {error}");
    }
    model_registry
}

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

pub async fn prepare_surface_inputs<F>(
    cli: &mut cli::Cli,
    config: &Config,
    auth: &AuthStorage,
    cwd: &Path,
    list_models: F,
) -> Result<Option<PreparedSurfaceInputs>>
where
    F: FnOnce(&ModelRegistry, Option<&str>),
{
    let global_dir = Config::global_dir();
    let models_path = crate::models::default_models_path(&global_dir);
    let model_registry = load_model_registry_with_warning(auth, &models_path);
    if let Some(pattern) = &cli.list_models {
        list_models(&model_registry, pattern.as_deref());
        return Ok(None);
    }

    if cli.mode.as_deref() != Some("rpc") {
        let stdin_content = read_piped_stdin()?;
        crate::app::apply_piped_stdin(cli, stdin_content);
    }

    if !cli.print && cli.mode.is_none() && !cli.message_args().is_empty() {
        cli.print = true;
    }

    crate::app::normalize_cli(cli);

    if let Some(export_path) = cli.export.clone() {
        let output = cli.message_args().first().map(ToString::to_string);
        let output_path =
            crate::surface::export_session_html(&export_path, output.as_deref()).await?;
        println!("Exported to: {}", output_path.display());
        return Ok(None);
    }

    crate::app::validate_rpc_args(cli)?;

    let mut messages: Vec<String> = cli.message_args().iter().map(ToString::to_string).collect();
    let file_args: Vec<String> = cli.file_args().iter().map(ToString::to_string).collect();
    let initial = crate::app::prepare_initial_message(
        cwd,
        &file_args,
        &mut messages,
        config
            .images
            .as_ref()
            .and_then(|i| i.auto_resize)
            .unwrap_or(true),
    )?;

    let surface_bootstrap = crate::surface::CliSurfaceBootstrap::from_cli(
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
        crate::app::parse_models_arg(models_arg)
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

/// Attempt to run interactive first-time setup to recover from a startup error.
/// Returns `None` recovery if the surface cannot run interactive setup.
pub async fn try_run_surface_startup_setup(
    surface_bootstrap: &crate::surface::CliSurfaceBootstrap,
    startup_error: &StartupError,
    auth: &mut AuthStorage,
    cli: &mut cli::Cli,
    models_path: &Path,
) -> Result<SurfaceStartupRecovery> {
    if !surface_bootstrap.can_run_interactive_setup() {
        return Ok(SurfaceStartupRecovery::None);
    }

    if run_first_time_setup(startup_error, auth, cli, models_path).await? {
        return Ok(SurfaceStartupRecovery::RetryAfterSetup);
    }

    Ok(SurfaceStartupRecovery::ExitQuietly)
}

/// Handle a startup error by attempting interactive setup or enforcing non-interactive guards.
pub async fn handle_surface_startup_error(
    surface_bootstrap: &crate::surface::CliSurfaceBootstrap,
    non_interactive_guard: Option<&crate::surface::NonInteractiveGuard>,
    startup_error: &StartupError,
    auth: &mut AuthStorage,
    cli: &mut cli::Cli,
    models_path: &Path,
    non_interactive_requirement: NonInteractiveStartupRequirement<'_>,
) -> Result<SurfaceStartupRecovery> {
    let recovery =
        try_run_surface_startup_setup(surface_bootstrap, startup_error, auth, cli, models_path)
            .await?;
    if !matches!(recovery, SurfaceStartupRecovery::None) {
        return Ok(recovery);
    }

    if let Some(guard) = non_interactive_guard {
        match non_interactive_requirement {
            NonInteractiveStartupRequirement::None => {}
            NonInteractiveStartupRequirement::Tty(operation) => {
                guard.check_tty_required(operation)?;
            }
            NonInteractiveStartupRequirement::OAuth(provider) => {
                guard.check_oauth_required(provider)?;
            }
        }
    }

    Ok(SurfaceStartupRecovery::None)
}

/// Recover from a startup error during model selection and determine the loop action.
pub async fn recover_surface_startup_error_for_selection(
    surface_bootstrap: &crate::surface::CliSurfaceBootstrap,
    non_interactive_guard: Option<&crate::surface::NonInteractiveGuard>,
    startup_error: &StartupError,
    auth: &mut AuthStorage,
    cli: &mut cli::Cli,
    models_path: &Path,
    model_registry: &mut ModelRegistry,
    non_interactive_requirement: NonInteractiveStartupRequirement<'_>,
) -> Result<SurfaceStartupLoopAction> {
    match handle_surface_startup_error(
        surface_bootstrap,
        non_interactive_guard,
        startup_error,
        auth,
        cli,
        models_path,
        non_interactive_requirement,
    )
    .await?
    {
        SurfaceStartupRecovery::RetryAfterSetup => {
            *model_registry = load_model_registry_with_warning(auth, models_path);
            Ok(SurfaceStartupLoopAction::ContinueSelection)
        }
        SurfaceStartupRecovery::ExitQuietly => Ok(SurfaceStartupLoopAction::ExitQuietly),
        SurfaceStartupRecovery::None => Ok(SurfaceStartupLoopAction::Propagate),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve_surface_model_selection(
    surface_bootstrap: &crate::surface::CliSurfaceBootstrap,
    non_interactive_guard: Option<&crate::surface::NonInteractiveGuard>,
    auth: &mut AuthStorage,
    cli: &mut cli::Cli,
    models_path: &Path,
    model_registry: &mut ModelRegistry,
    scoped_patterns: &[String],
    global_dir: &Path,
    config: &Config,
    session: &Session,
) -> Result<SurfaceSelectionOutcome> {
    let mut scoped_models: Vec<ScopedModel> = if scoped_patterns.is_empty() {
        Vec::new()
    } else {
        crate::app::resolve_model_scope(scoped_patterns, model_registry, cli.api_key.is_some())
    };

    if cli.api_key.is_some()
        && cli.provider.is_none()
        && cli.model.is_none()
        && scoped_models.is_empty()
    {
        anyhow::bail!(
            "--api-key requires a model to be specified via --provider/--model or --models"
        );
    }

    loop {
        scoped_models = if scoped_patterns.is_empty() {
            Vec::new()
        } else {
            crate::app::resolve_model_scope(scoped_patterns, model_registry, cli.api_key.is_some())
        };

        let selection = match crate::app::select_model_and_thinking(
            cli,
            config,
            session,
            model_registry,
            &scoped_models,
            global_dir,
        ) {
            Ok(selection) => selection,
            Err(err) => {
                if let Some(startup) = err.downcast_ref::<StartupError>() {
                    match recover_surface_startup_error_for_selection(
                        surface_bootstrap,
                        non_interactive_guard,
                        startup,
                        auth,
                        cli,
                        models_path,
                        model_registry,
                        NonInteractiveStartupRequirement::Tty("first-time setup"),
                    )
                    .await?
                    {
                        SurfaceStartupLoopAction::ContinueSelection => continue,
                        SurfaceStartupLoopAction::ExitQuietly => {
                            return Ok(SurfaceSelectionOutcome::ExitQuietly);
                        }
                        SurfaceStartupLoopAction::Propagate => {}
                    }
                }
                return Err(err);
            }
        };

        match crate::app::resolve_api_key(auth, cli, &selection.model_entry) {
            Ok(key) => {
                return Ok(SurfaceSelectionOutcome::Selected(
                    SurfaceSelectionResolution {
                        selection,
                        resolved_key: key,
                    },
                ));
            }
            Err(err) => {
                if let Some(startup) = err.downcast_ref::<StartupError>() {
                    if let StartupError::MissingApiKey { provider } = startup {
                        let canonical_provider =
                            crate::provider_metadata::canonical_provider_id(provider)
                                .unwrap_or(provider.as_str());
                        if canonical_provider == "sap-ai-core"
                            && let Some(token) =
                                crate::auth::exchange_sap_access_token(auth).await?
                        {
                            return Ok(SurfaceSelectionOutcome::Selected(
                                SurfaceSelectionResolution {
                                    selection,
                                    resolved_key: Some(token),
                                },
                            ));
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
                        surface_bootstrap,
                        non_interactive_guard,
                        startup,
                        auth,
                        cli,
                        models_path,
                        model_registry,
                        non_interactive_requirement,
                    )
                    .await?
                    {
                        SurfaceStartupLoopAction::ContinueSelection => continue,
                        SurfaceStartupLoopAction::ExitQuietly => {
                            return Ok(SurfaceSelectionOutcome::ExitQuietly);
                        }
                        SurfaceStartupLoopAction::Propagate => {}
                    }
                }
                return Err(err);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn build_surface_agent_session(
    cli: &cli::Cli,
    config: &Config,
    cwd: &Path,
    selection: &ModelSelection,
    resolved_key: Option<String>,
    mut session: Session,
    model_registry: &mut ModelRegistry,
    auth: &mut AuthStorage,
    enabled_tools: &[&str],
    skills_prompt: Option<&str>,
    global_dir: &Path,
    package_dir: &Path,
    test_mode: bool,
    extension_flags: &[cli::ExtensionCliFlag],
    extension_entries: &[PathBuf],
    extension_prewarm_handle: Option<SurfaceExtensionPrewarmHandle>,
) -> Result<AgentSession> {
    crate::app::update_session_for_selection(&mut session, selection);

    if let Some(message) = &selection.fallback_message {
        eprintln!("Warning: {message}");
    }

    let system_prompt = crate::app::build_system_prompt(
        cli,
        cwd,
        enabled_tools,
        skills_prompt,
        global_dir,
        package_dir,
        test_mode,
    );
    let provider =
        providers::create_provider(&selection.model_entry, None).map_err(anyhow::Error::new)?;
    let stream_options =
        crate::app::build_stream_options(config, resolved_key, selection, &session);
    let agent_config = AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations: 50,
        stream_options,
        block_images: config.image_block_images(),
    };

    let tools = ToolRegistry::new(enabled_tools, cwd, Some(config));
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
        enabled_tools,
        cwd,
        config,
        extension_entries,
        extension_flags,
        auth,
        model_registry,
        cli.extension_policy.as_deref(),
        cli.repair_policy.as_deref(),
        extension_prewarm_handle,
    )
    .await?;

    agent_session.set_model_registry(model_registry.clone());
    agent_session.set_auth_storage(auth.clone());

    Ok(agent_session)
}

async fn restore_session_history(agent_session: &mut AgentSession) -> Result<()> {
    let history = {
        let cx = crate::agent_cx::AgentCx::for_request();
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
