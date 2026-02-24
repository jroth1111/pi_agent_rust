//! Cross-platform sandboxing for command execution.
//!
//! This module provides OS-level sandboxing for bash commands using:
//! - **Linux**: bubblewrap + Landlock (kernel 5.13+)
//! - **macOS**: sandbox-exec (Seatbelt framework)
//! - **Windows**: Pending implementation

use std::path::Path;

use crate::error::{Error, Result};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub mod network;
pub mod verifier;

// Re-export main types for convenience
pub use network::{NetworkPolicy, NetworkPolicyBuilder};
pub use verifier::{CleanRoomVerifier, ConstraintSet, VerificationResult, VerifyPlan};

// Platform-specific exports
#[cfg(target_os = "linux")]
pub use linux::{FsAccess, LandlockRules};

#[cfg(target_os = "macos")]
pub use macos::{NetworkAllowlist, SeatbeltRules, TrustLevel};

/// Sandbox rules for controlling command execution isolation.
///
/// This enum provides a unified interface for configuring sandbox rules
/// across different platforms. Each variant is platform-specific and
/// uses the best available sandboxing technology for that platform.
#[derive(Debug, Clone, Default)]
pub enum SandboxRules {
    /// No sandboxing - direct execution.
    None,

    /// Platform-specific default sandboxing.
    ///
    /// Uses the best available sandboxing technology for the current platform:
    /// - Linux: bubblewrap with network isolation
    /// - macOS: sandbox-exec with standard trust level
    /// - Windows: No sandboxing (fallback to direct execution)
    #[default]
    Default,

    /// Linux-specific Landlock rules.
    #[cfg(target_os = "linux")]
    Linux(LandlockRules),

    /// macOS-specific Seatbelt rules.
    #[cfg(target_os = "macos")]
    MacOs(SeatbeltRules),
}

impl SandboxRules {
    /// Create rules for no sandboxing.
    pub const fn none() -> Self {
        Self::None
    }

    /// Create rules for default platform sandboxing.
    pub const fn default_rules() -> Self {
        Self::Default
    }

    /// Create Linux-specific Landlock rules.
    #[cfg(target_os = "linux")]
    pub fn linux(rules: LandlockRules) -> Self {
        Self::Linux(rules)
    }

    /// Create macOS-specific Seatbelt rules.
    #[cfg(target_os = "macos")]
    pub const fn macos(rules: SeatbeltRules) -> Self {
        Self::MacOs(rules)
    }

    /// Check if sandboxing is enabled for these rules.
    pub const fn is_sandboxed(&self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Debug, Clone)]
pub struct BashSpawnPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env_overrides: Vec<(String, String)>,
    pub sandboxed: bool,
    pub sandbox_wrapper: Option<String>,
    pub fallback_note: Option<String>,
    /// Sandbox rules that were applied to this plan.
    pub sandbox_rules: Option<SandboxRules>,
}

impl BashSpawnPlan {
    pub fn direct(shell: &str, command: &str, fallback_note: Option<String>) -> Self {
        Self {
            program: shell.to_string(),
            args: vec!["-c".to_string(), command.to_string()],
            env_overrides: Vec::new(),
            sandboxed: false,
            sandbox_wrapper: None,
            fallback_note,
            sandbox_rules: Some(SandboxRules::None),
        }
    }

    /// Create a sandboxed plan with custom rules.
    pub fn with_rules(
        cwd: &Path,
        shell: &str,
        command: &str,
        rules: &SandboxRules,
    ) -> Result<Self> {
        match rules {
            SandboxRules::None => Ok(Self::direct(shell, command, None)),
            SandboxRules::Default => build_bash_spawn_plan(cwd, shell, command),

            #[cfg(target_os = "linux")]
            SandboxRules::Linux(landlock_rules) => {
                if !linux::wrapper_available() {
                    return Err(Error::tool(
                        "bash",
                        "Linux sandbox (bwrap) is unavailable or unusable".to_string(),
                    ));
                }
                let mut plan = linux::build_plan_with_rules(cwd, shell, command, landlock_rules);
                plan.sandbox_rules = Some(rules.clone());
                Ok(plan)
            }

            #[cfg(target_os = "macos")]
            SandboxRules::MacOs(seatbelt_rules) => {
                if !macos::wrapper_available() {
                    return Err(Error::tool(
                        "bash",
                        "macOS sandbox (sandbox-exec) is unavailable or unusable".to_string(),
                    ));
                }
                let mut plan = seatbelt_rules.build_plan(cwd, shell, command);
                plan.sandbox_rules = Some(rules.clone());
                Ok(plan)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BashSandboxMode {
    Off,
    Auto,
    Strict,
}

impl BashSandboxMode {
    fn from_env() -> Self {
        match std::env::var("PI_BASH_SANDBOX")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("off" | "0" | "false" | "no") => Self::Off,
            Some("strict") => Self::Strict,
            _ => Self::Auto,
        }
    }
}

pub fn build_bash_spawn_plan(cwd: &Path, shell: &str, command: &str) -> Result<BashSpawnPlan> {
    let mode = BashSandboxMode::from_env();
    if mode == BashSandboxMode::Off {
        return Ok(BashSpawnPlan::direct(shell, command, None));
    }

    #[cfg(target_os = "macos")]
    {
        if macos::wrapper_available() {
            let mut plan = macos::build_plan(cwd, shell, command);
            plan.sandbox_rules = Some(SandboxRules::Default);
            return Ok(plan);
        }
        if mode == BashSandboxMode::Strict {
            return Err(Error::tool(
                "bash",
                "PI_BASH_SANDBOX=strict but sandbox-exec is unavailable or unusable".to_string(),
            ));
        }
        let mut plan = BashSpawnPlan::direct(
            shell,
            command,
            Some(
                "OS sandbox wrapper unavailable on this machine; command ran without OS wrapper."
                    .to_string(),
            ),
        );
        plan.sandbox_rules = Some(SandboxRules::None);
        Ok(plan)
    }

    #[cfg(target_os = "linux")]
    {
        if linux::wrapper_available() {
            let mut plan = linux::build_plan(cwd, shell, command);
            plan.sandbox_rules = Some(SandboxRules::Default);
            return Ok(plan);
        }
        if mode == BashSandboxMode::Strict {
            return Err(Error::tool(
                "bash",
                "PI_BASH_SANDBOX=strict but bwrap is unavailable or unusable".to_string(),
            ));
        }
        let mut plan = BashSpawnPlan::direct(
            shell,
            command,
            Some(
                "OS sandbox wrapper unavailable on this machine; command ran without OS wrapper."
                    .to_string(),
            ),
        );
        plan.sandbox_rules = Some(SandboxRules::None);
        Ok(plan)
    }

    #[cfg(target_os = "windows")]
    {
        if windows::wrapper_available() {
            let mut plan = windows::build_plan(cwd, shell, command);
            plan.sandbox_rules = Some(SandboxRules::Default);
            return Ok(plan);
        }
        if mode == BashSandboxMode::Strict {
            return Err(Error::tool(
                "bash",
                "PI_BASH_SANDBOX=strict is not supported on Windows yet".to_string(),
            ));
        }
        let mut plan = BashSpawnPlan::direct(
            shell,
            command,
            Some(
                "OS sandbox wrappers are not supported on this OS yet; command ran without OS wrapper."
                    .to_string(),
            ),
        );
        plan.sandbox_rules = Some(SandboxRules::None);
        Ok(plan)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        if mode == BashSandboxMode::Strict {
            return Err(Error::tool(
                "bash",
                "PI_BASH_SANDBOX=strict is not supported on this OS yet".to_string(),
            ));
        }
        let mut plan = BashSpawnPlan::direct(
            shell,
            command,
            Some(
                "OS sandbox wrappers are not supported on this OS yet; command ran without OS wrapper."
                    .to_string(),
            ),
        );
        plan.sandbox_rules = Some(SandboxRules::None);
        Ok(plan)
    }
}
