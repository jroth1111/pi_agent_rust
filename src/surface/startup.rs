//! Startup recovery and error handling for CLI surfaces.
//!
//! Contains the startup recovery enums and functions that orchestrate
//! first-time setup, auth error recovery, and model selection retry logic.
//! Lives in the surface layer because it is purely startup I/O coordination —
//! no inference, session state, or business-rule ownership.

use std::path::Path;

use anyhow::Result;

use crate::app::StartupError;
use crate::auth::AuthStorage;
use crate::cli;
use crate::models::ModelRegistry;
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
