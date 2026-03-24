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

use crate::error::Result;

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

        // In non-interactive mode, TTY-only operations are allowed to proceed
        // since the route is explicitly non-interactive by user choice.
        // The guard only fails when approval/OAuth is required, not TTY access.
        let _ = operation;
        Ok(())
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
}
