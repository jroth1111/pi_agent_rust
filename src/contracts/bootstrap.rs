//! Surface bootstrap contracts for thin entrypoints.
//!
//! CLI, TUI, RPC, and SDK surfaces should describe startup intent with these
//! types and then hand off to an application kernel. This keeps runtime
//! bootstrapping explicit instead of letting each surface synthesize its own
//! ad hoc startup semantics.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// External surface initiating the runtime.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    Cli,
    Interactive,
    Rpc,
    Sdk,
    TestHarness,
}

impl SurfaceKind {
    #[must_use]
    pub const fn is_interactive(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

/// Intended runtime interaction model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InteractionMode {
    Interactive,
    Headless,
    Batch,
}

impl InteractionMode {
    #[must_use]
    pub const fn allows_prompts(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

/// Filesystem roots resolved before application startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BootstrapPaths {
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_root: Option<PathBuf>,
}

/// Surface capabilities that affect safe startup behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SurfaceCapabilities {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub allow_prompts: bool,
    pub allow_browser_auth: bool,
    pub allow_approval_ui: bool,
}

/// Resume intent carried across compaction, restart, or handoff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ResumeTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

impl ResumeTarget {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.session_id.is_none() && self.run_id.is_none() && self.task_id.is_none()
    }
}

/// Runtime services required for a particular surface startup path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapProfile {
    pub require_conversation: bool,
    pub require_workflow: bool,
    pub require_workspace: bool,
    pub require_host_execution: bool,
    pub require_extension_runtime: bool,
}

impl BootstrapProfile {
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            require_conversation: true,
            require_workflow: true,
            require_workspace: true,
            require_host_execution: true,
            require_extension_runtime: true,
        }
    }

    #[must_use]
    pub const fn rpc() -> Self {
        Self {
            require_conversation: true,
            require_workflow: true,
            require_workspace: true,
            require_host_execution: true,
            require_extension_runtime: false,
        }
    }

    #[must_use]
    pub const fn batch() -> Self {
        Self {
            require_conversation: true,
            require_workflow: true,
            require_workspace: true,
            require_host_execution: true,
            require_extension_runtime: false,
        }
    }
}

/// Canonical startup envelope for surface-to-kernel handoff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapRequest {
    pub surface: SurfaceKind,
    pub interaction_mode: InteractionMode,
    pub profile: BootstrapProfile,
    pub paths: BootstrapPaths,
    pub capabilities: SurfaceCapabilities,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub env_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub resume: ResumeTarget,
}

impl BootstrapRequest {
    #[must_use]
    pub fn for_surface(
        surface: SurfaceKind,
        interaction_mode: InteractionMode,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        let profile = match surface {
            SurfaceKind::Interactive => BootstrapProfile::interactive(),
            SurfaceKind::Rpc => BootstrapProfile::rpc(),
            SurfaceKind::Cli | SurfaceKind::Sdk | SurfaceKind::TestHarness => {
                BootstrapProfile::batch()
            }
        };

        Self {
            surface,
            interaction_mode,
            profile,
            paths: BootstrapPaths {
                cwd: cwd.into(),
                ..BootstrapPaths::default()
            },
            capabilities: SurfaceCapabilities::default(),
            argv: Vec::new(),
            env_overrides: BTreeMap::new(),
            resume: ResumeTarget::default(),
        }
    }

    /// Validate that the surface claims are internally coherent.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the request describes an impossible or
    /// contradictory startup mode.
    pub fn validate(&self) -> Result<()> {
        if self.paths.cwd.as_os_str().is_empty() {
            return Err(Error::validation("bootstrap request cwd is empty"));
        }

        if self.surface.is_interactive() && self.interaction_mode != InteractionMode::Interactive {
            return Err(Error::validation(
                "interactive surface must use interactive interaction_mode",
            ));
        }

        if self.interaction_mode.allows_prompts() && !self.capabilities.allow_prompts {
            return Err(Error::validation(
                "interactive bootstrap requires prompt-capable surface",
            ));
        }

        if self.capabilities.allow_browser_auth && !self.capabilities.allow_prompts {
            return Err(Error::validation(
                "browser auth requires prompt-capable surface coordination",
            ));
        }

        if self.capabilities.allow_approval_ui && !self.capabilities.allow_prompts {
            return Err(Error::validation(
                "approval UI requires prompt-capable surface coordination",
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_surface_requires_interactive_mode() {
        let mut request = BootstrapRequest::for_surface(
            SurfaceKind::Interactive,
            InteractionMode::Batch,
            "/tmp/project",
        );
        request.capabilities.allow_prompts = true;

        let err = request.validate().expect_err("validation should fail");
        assert!(err.to_string().contains("interactive surface"));
    }

    #[test]
    fn prompting_modes_require_prompt_capability() {
        let request = BootstrapRequest::for_surface(
            SurfaceKind::Cli,
            InteractionMode::Interactive,
            "/tmp/project",
        );

        let err = request.validate().expect_err("validation should fail");
        assert!(err.to_string().contains("prompt-capable"));
    }

    #[test]
    fn browser_auth_requires_prompt_capability() {
        let mut request =
            BootstrapRequest::for_surface(SurfaceKind::Cli, InteractionMode::Batch, "/tmp");
        request.capabilities.allow_browser_auth = true;

        let err = request.validate().expect_err("validation should fail");
        assert!(err.to_string().contains("browser auth"));
    }

    #[test]
    fn well_formed_interactive_request_validates() {
        let mut request = BootstrapRequest::for_surface(
            SurfaceKind::Interactive,
            InteractionMode::Interactive,
            "/tmp/project",
        );
        request.capabilities = SurfaceCapabilities {
            stdin_tty: true,
            stdout_tty: true,
            allow_prompts: true,
            allow_browser_auth: true,
            allow_approval_ui: true,
        };
        request.resume.session_id = Some("session-1".to_string());

        request.validate().expect("request should validate");
        assert!(!request.resume.is_empty());
        assert!(request.profile.require_extension_runtime);
    }
}
