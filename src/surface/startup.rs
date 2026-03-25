//! Startup recovery and error handling for CLI surfaces.
//!
//! Contains the startup recovery enums and functions that orchestrate
//! first-time setup, auth error recovery, and model selection retry logic.
//! Lives in the surface layer because it is purely startup I/O coordination —
//! no inference, session state, or business-rule ownership.

use std::path::Path;

use anyhow::Result;

use crate::app::{ModelSelection, ScopedModel, StartupError};
use crate::auth::AuthStorage;
use crate::cli;
use crate::config::Config;
use crate::models::ModelRegistry;
use crate::session::Session;
use crate::surface::auth_setup::run_first_time_setup;

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
