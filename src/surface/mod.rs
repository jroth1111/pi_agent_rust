//! CLI surface adapters over typed service contracts.
//!
//! This module provides the thin adapter layer between the CLI (main.rs, CLI handlers)
//! and the typed service contracts defined in `src/contracts/`. The adapters
//! ensure that CLI surfaces do not own business-state decisions and delegate
//! to services instead.
//!
//! ## Design Goals
//!
//! 1. **Composition-only**: CLI parses arguments and delegates to services
//! 2. **Fail-closed**: Non-interactive routes fail explicitly when approval is required
//! 3. **No direct engine access**: CLI goes through contracts
//!
//! # Non-interactive Route Guard
//!
//! The `NonInteractiveGuard` ensures that routes marked as non-interactive
//! fail closed when they would require interactive approval flows. This prevents
//! silent fallback to interactive mode.

pub mod auth_setup;
pub mod extension_policy;
pub mod extension_runtime;
pub mod routing;
pub mod rpc_protocol;
pub mod rpc_runtime_commands;
pub mod rpc_server;
pub mod rpc_service_commands;
pub mod rpc_session_commands;
pub mod rpc_support;
pub mod rpc_transport_commands;
pub mod startup;

use crate::agent::AgentSession;
use crate::contracts::bootstrap::{
    BootstrapRequest, InteractionMode, SurfaceCapabilities, SurfaceKind,
};
use crate::error::Result;
use crate::extensions::{ExtensionUiRequest, ExtensionUiResponse};
use crate::session::Session;
use asupersync::runtime::RuntimeHandle;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

/// Guard that ensures non-interactive routes fail closed instead of entering interactive flows.
///
/// When a non-interactive route encounters a situation that would require interactive approval
/// (e.g., extension capability prompt, OAuth login, or TTY-only confirmation), the guard
/// returns an explicit error instead of silently switching to interactive mode.
#[derive(Debug, Clone)]
pub struct NonInteractiveGuard {
    /// Whether this route is marked as non-interactive.
    pub is_non_interactive: bool,
    /// Reason for blocked approval, if any.
    pub blocked_reason: Option<String>,
}

impl Default for NonInteractiveGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolved CLI surface bootstrap plan.
#[derive(Debug, Clone)]
pub struct CliSurfaceBootstrap {
    pub route_kind: CliRouteKind,
    pub bootstrap_request: BootstrapRequest,
    pub mode: String,
}

impl NonInteractiveGuard {
    /// Create a new guard for a non-interactive route.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            is_non_interactive: true,
            blocked_reason: None,
        }
    }

    /// Check if approval is required and fail closed if so.
    ///
    /// # Errors
    ///
    /// Returns a validation error if this is a non-interactive route and approval is required.
    pub fn check_approval_required(&self, capability: &str, _context: &str) -> Result<()> {
        if !self.is_non_interactive {
            return Ok(());
        }

        // In non-interactive mode, any approval requirement is a hard failure.
        Err(crate::error::Error::validation(format!(
            "Approval required for '{}' in non-interactive mode. \
            This operation requires interactive approval but the current route is non-interactive. \
            Use interactive mode (remove --print or --mode flags) to approve this operation, \
            or configure non-interactive approval via environment variables or config.",
            capability
        )))
    }

    /// Check if an OAuth flow is required and fail closed if so.
    ///
    /// # Errors
    ///
    /// Returns a validation error if this is a non-interactive route and OAuth is required.
    pub fn check_oauth_required(&self, provider: &str) -> Result<()> {
        if !self.is_non_interactive {
            return Ok(());
        }

        // In non-interactive mode, OAuth flows are not supported.
        Err(crate::error::Error::validation(format!(
            "OAuth login required for '{}' in non-interactive mode. \
            OAuth flows require interactive browser access. \
            Use interactive mode or pre-configure credentials via environment variables.",
            provider
        )))
    }

    /// Check if TTY is required and fail closed if not available in non-interactive mode.
    ///
    /// # Errors
    ///
    /// Returns a validation error if this is a non-interactive route, TTY is required,
    /// but stdin/stdout are not TTYs.
    pub fn check_tty_required(&self, operation: &str) -> Result<()> {
        if !self.is_non_interactive {
            return Ok(());
        }

        Err(crate::error::Error::validation(format!(
            "TTY access required for '{}' in non-interactive mode. \
            This operation requires an interactive terminal but the current route is non-interactive. \
            Use interactive mode without --print/--mode flags, or complete the setup/configuration beforehand.",
            operation
        )))
    }

    /// Check whether an extension UI request can be served on a non-interactive route.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the requested extension UI interaction
    /// would require approval or a terminal on a non-interactive route.
    pub fn check_extension_ui_request(
        &self,
        method: &str,
        capability: Option<&str>,
        title: Option<&str>,
    ) -> Result<()> {
        let title = title.map(str::trim).filter(|value| !value.is_empty());
        let capability = capability
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(title);

        match method.trim() {
            "confirm" => self.check_approval_required(
                capability.unwrap_or("extension confirmation"),
                "extension_ui",
            ),
            "select" | "input" | "editor" => {
                self.check_tty_required(capability.unwrap_or("extension UI request"))
            }
            _ => Ok(()),
        }
    }

    /// Mark the guard as having a blocked approval.
    pub fn mark_blocked(&mut self, reason: &str) {
        self.blocked_reason = Some(reason.to_string());
    }

    /// Check if approval was blocked.
    #[must_use]
    pub const fn is_blocked(&self) -> bool {
        self.blocked_reason.is_some()
    }
}

impl CliSurfaceBootstrap {
    /// Resolve CLI flags into a typed route/bootstrap plan.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the synthesized bootstrap request is incoherent.
    pub fn from_cli(
        mode: Option<&str>,
        print: bool,
        cwd: impl Into<PathBuf>,
        stdin_tty: bool,
        stdout_tty: bool,
        argv: Vec<String>,
    ) -> Result<Self> {
        let route_kind = if mode == Some("rpc") {
            CliRouteKind::Rpc
        } else if !print && mode.is_none() {
            CliRouteKind::Interactive
        } else if print {
            CliRouteKind::Print
        } else if mode == Some("json") {
            CliRouteKind::Json
        } else {
            CliRouteKind::Text
        };

        let bootstrap_request = route_kind.bootstrap_request(cwd, stdin_tty, stdout_tty, argv);
        bootstrap_request.validate()?;

        Ok(Self {
            route_kind,
            bootstrap_request,
            mode: mode.unwrap_or("text").to_string(),
        })
    }

    #[must_use]
    pub const fn is_interactive(&self) -> bool {
        matches!(
            self.bootstrap_request.interaction_mode,
            InteractionMode::Interactive
        )
    }

    #[must_use]
    pub const fn non_interactive_guard(&self) -> Option<NonInteractiveGuard> {
        self.route_kind.non_interactive_guard()
    }

    #[must_use]
    pub const fn has_tty_access(&self) -> bool {
        self.bootstrap_request.capabilities.stdin_tty
            && self.bootstrap_request.capabilities.stdout_tty
    }

    #[must_use]
    pub const fn can_run_interactive_setup(&self) -> bool {
        self.is_interactive() && self.has_tty_access()
    }
}

/// Export a session transcript to HTML for CLI surface flows.
///
/// # Errors
///
/// Returns an error when the input session path does not exist, the session
/// cannot be opened, or the output file cannot be written.
pub async fn export_session_html(input_path: &str, output_path: Option<&str>) -> Result<PathBuf> {
    let input = Path::new(input_path);
    if !input.exists() {
        return Err(crate::error::Error::validation(format!(
            "File not found: {input_path}"
        )));
    }

    let session = Session::open(input_path).await?;
    let html = crate::app::render_session_html(&session);
    let output_path = output_path.map_or_else(|| default_export_path(input), PathBuf::from);

    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&output_path, html)?;
    Ok(output_path)
}

fn default_export_path(input: &Path) -> PathBuf {
    let basename = input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session");
    PathBuf::from(format!("pi-session-{basename}.html"))
}

/// Install a fail-closed bridge for extension UI on non-interactive routes.
///
/// The returned slot records the first blocked reason observed by the bridge so
/// callers can convert it into a startup/runtime error at a deterministic boundary.
#[must_use]
pub fn install_non_interactive_extension_ui_bridge(
    session: &AgentSession,
    runtime_handle: &RuntimeHandle,
) -> Option<Arc<StdMutex<Option<String>>>> {
    let manager = session
        .extensions
        .as_ref()
        .map(|region| region.manager().clone())?;
    let (extension_ui_tx, extension_ui_rx) =
        asupersync::channel::mpsc::channel::<ExtensionUiRequest>(64);
    manager.set_ui_sender(extension_ui_tx);

    let blocked_reason = Arc::new(StdMutex::new(None));
    let blocked_reason_task = Arc::clone(&blocked_reason);
    let manager_ui = manager.clone();
    let runtime_handle_ui = runtime_handle.clone();
    runtime_handle.spawn(async move {
        let cx = crate::agent_cx::AgentCx::for_request();
        let guard = NonInteractiveGuard::new();
        while let Ok(request) = extension_ui_rx.recv(&cx).await {
            if let Err(err) = check_non_interactive_extension_ui_request(&guard, &request) {
                let mut slot = blocked_reason_task
                    .lock()
                    .expect("non-interactive extension UI blocked reason lock");
                if slot.is_none() {
                    *slot = Some(err.to_string());
                }
            }

            if request.expects_response() {
                let _ = manager_ui.respond_ui(ExtensionUiResponse {
                    id: request.id.clone(),
                    value: None,
                    cancelled: true,
                });
            }
        }

        drop(runtime_handle_ui);
    });

    Some(blocked_reason)
}

/// Check whether a queued extension UI request should fail closed on a
/// non-interactive route.
///
/// # Errors
///
/// Returns a validation error when the request requires approval or TTY access.
pub fn check_non_interactive_extension_ui_request(
    guard: &NonInteractiveGuard,
    request: &ExtensionUiRequest,
) -> Result<()> {
    let capability = request.payload.get("capability").and_then(Value::as_str);
    let title = request
        .payload
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| request.payload.get("message").and_then(Value::as_str));
    guard.check_extension_ui_request(&request.method, capability, title)
}

/// Convert the first blocked extension UI reason into a runtime error.
///
/// # Errors
///
/// Returns the recorded blocked reason, if any.
pub fn check_non_interactive_extension_ui_blocked(
    blocked_reason: &Option<Arc<StdMutex<Option<String>>>>,
) -> Result<()> {
    let Some(blocked_reason) = blocked_reason else {
        return Ok(());
    };
    {
        let mut guard = blocked_reason
            .lock()
            .expect("non-interactive extension UI blocked reason lock");
        if let Some(message) = guard.take() {
            return Err(crate::error::Error::validation(message));
        }
    }
    Ok(())
}

/// CLI route classification for determining interactive vs non-interactive behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliRouteKind {
    /// Interactive TUI mode (default, no --print or --mode flags)
    Interactive,
    /// RPC mode (--mode rpc)
    Rpc,
    /// Print mode (--print)
    Print,
    /// JSON mode (--mode json)
    Json,
    /// Text mode (--mode text)
    Text,
    /// Subcommand (install, Remove, Config, etc.)
    Subcommand,
    /// Fast offline path (--version, --list-providers, --help)
    FastOffline,
}

impl CliRouteKind {
    /// Determine if this route is non-interactive.
    #[must_use]
    pub const fn is_non_interactive(&self) -> bool {
        matches!(
            self,
            Self::Rpc
                | Self::Print
                | Self::Json
                | Self::Text
                | Self::Subcommand
                | Self::FastOffline
        )
    }

    /// Determine if this route requires streaming/inference.
    #[must_use]
    pub const fn requires_inference(&self) -> bool {
        matches!(
            self,
            Self::Interactive | Self::Rpc | Self::Print | Self::Json | Self::Text
        )
    }

    /// Map the CLI route kind to the surface kind used by bootstrap contracts.
    #[must_use]
    pub const fn surface_kind(&self) -> SurfaceKind {
        match self {
            Self::Interactive => SurfaceKind::Interactive,
            Self::Rpc => SurfaceKind::Rpc,
            Self::Print | Self::Json | Self::Text | Self::Subcommand | Self::FastOffline => {
                SurfaceKind::Cli
            }
        }
    }

    /// Map the CLI route kind to the intended interaction mode.
    #[must_use]
    pub const fn interaction_mode(&self) -> InteractionMode {
        match self {
            Self::Interactive => InteractionMode::Interactive,
            Self::Rpc => InteractionMode::Headless,
            Self::Print | Self::Json | Self::Text | Self::Subcommand | Self::FastOffline => {
                InteractionMode::Batch
            }
        }
    }

    /// Build a typed bootstrap request for the route.
    #[must_use]
    pub fn bootstrap_request(
        &self,
        cwd: impl Into<PathBuf>,
        stdin_tty: bool,
        stdout_tty: bool,
        argv: Vec<String>,
    ) -> BootstrapRequest {
        let mut request =
            BootstrapRequest::for_surface(self.surface_kind(), self.interaction_mode(), cwd);
        request.capabilities = SurfaceCapabilities {
            stdin_tty,
            stdout_tty,
            allow_prompts: !self.is_non_interactive(),
            allow_browser_auth: !self.is_non_interactive(),
            allow_approval_ui: !self.is_non_interactive(),
        };
        request.argv = argv;
        request
    }

    /// Construct a non-interactive guard when the route must fail closed.
    #[must_use]
    pub const fn non_interactive_guard(&self) -> Option<NonInteractiveGuard> {
        if self.is_non_interactive() {
            Some(NonInteractiveGuard::new())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_interactive_guard_approval_check() {
        let guard = NonInteractiveGuard::new();

        // Non-interactive guard should fail on approval requirement
        let result = guard.check_approval_required("test_capability", "test context");
        assert!(result.is_err());

        // Error message should mention non-interactive mode
        let err = result.unwrap_err();
        assert!(err.to_string().contains("non-interactive mode"));
    }

    #[test]
    fn non_interactive_guard_oauth_check() {
        let guard = NonInteractiveGuard::new();

        // Non-interactive guard should fail on OAuth requirement
        let result = guard.check_oauth_required("test_provider");
        assert!(result.is_err());

        // Error message should mention OAuth and interactive mode
        let err = result.unwrap_err();
        assert!(err.to_string().contains("OAuth"));
        assert!(err.to_string().contains("non-interactive"));
    }

    #[test]
    fn non_interactive_guard_tty_check() {
        let guard = NonInteractiveGuard::new();

        let result = guard.check_tty_required("first-time setup");
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(err.to_string().contains("TTY access required"));
        assert!(err.to_string().contains("non-interactive"));
    }

    #[test]
    fn non_interactive_guard_extension_confirm_check() {
        let guard = NonInteractiveGuard::new();

        let result = guard.check_extension_ui_request(
            "confirm",
            Some("filesystem.write"),
            Some("Allow capability"),
        );
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(err.to_string().contains("filesystem.write"));
        assert!(err.to_string().contains("Approval required"));
    }

    #[test]
    fn non_interactive_guard_extension_input_check() {
        let guard = NonInteractiveGuard::new();

        let result = guard.check_extension_ui_request("input", None, Some("Extension needs input"));
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(err.to_string().contains("TTY access required"));
        assert!(err.to_string().contains("Extension needs input"));
    }

    #[test]
    fn cli_route_kind_classification() {
        // Interactive routes
        assert!(!CliRouteKind::Interactive.is_non_interactive());
        assert!(CliRouteKind::Interactive.requires_inference());

        // Non-interactive routes
        assert!(CliRouteKind::Rpc.is_non_interactive());
        assert!(CliRouteKind::Print.is_non_interactive());
        assert!(CliRouteKind::Json.is_non_interactive());
        assert!(CliRouteKind::Text.is_non_interactive());
        assert!(CliRouteKind::Subcommand.is_non_interactive());
        assert!(CliRouteKind::FastOffline.is_non_interactive());

        // Inference requirement
        assert!(CliRouteKind::Rpc.requires_inference());
        assert!(CliRouteKind::Print.requires_inference());
        assert!(CliRouteKind::Json.requires_inference());
        assert!(CliRouteKind::Text.requires_inference());
        assert!(!CliRouteKind::Subcommand.requires_inference());
        assert!(!CliRouteKind::FastOffline.requires_inference());
    }

    #[test]
    fn route_kind_builds_bootstrap_request_with_expected_surface_traits() {
        let interactive = CliRouteKind::Interactive.bootstrap_request(
            "/tmp/project",
            true,
            true,
            vec!["pi".to_string()],
        );
        assert_eq!(interactive.surface, SurfaceKind::Interactive);
        assert_eq!(interactive.interaction_mode, InteractionMode::Interactive);
        assert!(interactive.capabilities.allow_prompts);
        assert!(interactive.validate().is_ok());

        let rpc = CliRouteKind::Rpc.bootstrap_request(
            "/tmp/project",
            false,
            false,
            vec!["pi".to_string(), "--mode".to_string(), "rpc".to_string()],
        );
        assert_eq!(rpc.surface, SurfaceKind::Rpc);
        assert_eq!(rpc.interaction_mode, InteractionMode::Headless);
        assert!(!rpc.capabilities.allow_prompts);
        assert!(rpc.validate().is_ok());
    }

    #[test]
    fn cli_surface_bootstrap_resolves_print_mode() {
        let bootstrap = CliSurfaceBootstrap::from_cli(
            Some("json"),
            true,
            "/tmp/project",
            false,
            false,
            vec!["pi".to_string(), "--print".to_string()],
        )
        .expect("print bootstrap should validate");

        assert_eq!(bootstrap.route_kind, CliRouteKind::Print);
        assert_eq!(bootstrap.mode, "json");
        assert!(!bootstrap.is_interactive());
        assert!(bootstrap.non_interactive_guard().is_some());
    }

    #[test]
    fn cli_surface_bootstrap_resolves_interactive_mode() {
        let bootstrap =
            CliSurfaceBootstrap::from_cli(None, false, "/tmp/project", true, true, vec![])
                .expect("interactive bootstrap should validate");

        assert_eq!(bootstrap.route_kind, CliRouteKind::Interactive);
        assert!(bootstrap.is_interactive());
        assert!(bootstrap.has_tty_access());
        assert!(bootstrap.can_run_interactive_setup());
        assert!(bootstrap.non_interactive_guard().is_none());
    }

    #[test]
    fn cli_surface_bootstrap_denies_interactive_setup_without_tty() {
        let bootstrap = CliSurfaceBootstrap::from_cli(
            None,
            false,
            "/tmp/project",
            true,
            false,
            vec!["pi".to_string()],
        )
        .expect("bootstrap should validate");

        assert!(bootstrap.is_interactive());
        assert!(!bootstrap.has_tty_access());
        assert!(!bootstrap.can_run_interactive_setup());
    }
}
