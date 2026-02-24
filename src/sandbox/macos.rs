//! macOS sandbox implementation using Seatbelt (sandbox-exec).
//!
//! This module provides OS-level sandboxing for macOS using the built-in
//! sandbox-exec facility, which is based on Apple's Seatbelt sandbox framework.

use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;

use super::BashSpawnPlan;

/// Trust level for the macOS sandbox profile.
///
/// Trust levels determine the default security posture of the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrustLevel {
    /// Restricted: Maximum isolation, minimal access.
    ///
    /// Network access is denied by default. Only explicitly allowed
    /// filesystem paths are accessible. Best for untrusted commands.
    Restricted,

    /// Standard: Balanced isolation for typical AI agent operations.
    ///
    /// Allows common operations like file I/O in the working directory
    /// and temp directory. Network access is denied by default but can
    /// be selectively enabled.
    #[default]
    Standard,

    /// Permissive: Relaxed isolation for trusted operations.
    ///
    /// Allows broader filesystem access and optional network access.
    /// Use only for trusted commands and development environments.
    Permissive,
}

/// Network allowlist entries for macOS sandbox.
///
/// The macOS sandbox supports network rules based on IP addresses and
/// (in some versions) domain names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkAllowlist {
    /// Allow outbound TCP connections to a specific IP.
    Tcp(String),
    /// Allow outbound UDP connections to a specific IP.
    Udp(String),
    /// Allow all outbound network (use with caution).
    All,
}

/// Seatbelt-compatible sandbox rules for macOS.
///
/// These rules are used to generate sandbox-exec profiles that control
/// filesystem, network, and process capabilities.
#[derive(Debug, Clone)]
pub struct SeatbeltRules {
    /// Trust level for the sandbox profile.
    pub trust_level: TrustLevel,
    /// Paths where file read operations are allowed.
    pub read_allowed_paths: Vec<String>,
    /// Paths where file write operations are allowed.
    pub write_allowed_paths: Vec<String>,
    /// Network allowlist entries.
    pub network_allowlist: Vec<NetworkAllowlist>,
    /// Whether to allow process execution.
    pub allow_process_exec: bool,
    /// Whether to allow process fork.
    pub allow_process_fork: bool,
}

impl Default for SeatbeltRules {
    fn default() -> Self {
        Self::new()
    }
}

impl SeatbeltRules {
    /// Create a new set of Seatbelt rules with standard trust level.
    pub const fn new() -> Self {
        Self {
            trust_level: TrustLevel::Standard,
            read_allowed_paths: Vec::new(),
            write_allowed_paths: Vec::new(),
            network_allowlist: Vec::new(),
            allow_process_exec: true,
            allow_process_fork: true,
        }
    }

    /// Create rules with restricted trust level.
    pub fn restricted() -> Self {
        Self {
            trust_level: TrustLevel::Restricted,
            ..Self::new()
        }
    }

    /// Create rules with permissive trust level.
    pub fn permissive() -> Self {
        Self {
            trust_level: TrustLevel::Permissive,
            ..Self::new()
        }
    }

    /// Add a path for read access.
    pub fn allow_read(mut self, path: impl Into<String>) -> Self {
        self.read_allowed_paths.push(path.into());
        self
    }

    /// Add a path for write access.
    pub fn allow_write(mut self, path: impl Into<String>) -> Self {
        self.write_allowed_paths.push(path.into());
        self
    }

    /// Add a path for both read and write access.
    pub fn allow_read_write(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.read_allowed_paths.push(path.clone());
        self.write_allowed_paths.push(path);
        self
    }

    /// Add a network allowlist entry.
    pub fn allow_network(mut self, entry: NetworkAllowlist) -> Self {
        self.network_allowlist.push(entry);
        self
    }

    /// Allow all network (use with caution).
    pub fn allow_network_all(mut self) -> Self {
        self.network_allowlist.push(NetworkAllowlist::All);
        self
    }

    /// Set process execution permissions.
    pub const fn with_process_exec(mut self, allowed: bool) -> Self {
        self.allow_process_exec = allowed;
        self
    }

    /// Set process fork permissions.
    pub const fn with_process_fork(mut self, allowed: bool) -> Self {
        self.allow_process_fork = allowed;
        self
    }

    /// Generate a sandbox-exec profile from these rules.
    fn build_profile(&self, cwd: &Path) -> String {
        fn quote(path: &Path) -> String {
            path.display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        }

        let cwd_quoted = quote(cwd);
        let tmp_quoted = quote(&std::env::temp_dir());

        let mut profile = String::from("(version 1)\n");

        // Base policy depends on trust level
        match self.trust_level {
            TrustLevel::Restricted => {
                profile.push_str("(deny default)\n");
                profile.push_str("(import \"system.sb\")\n");
                profile.push_str("(allow process*)\n");
            }
            TrustLevel::Standard => {
                profile.push_str("(deny default)\n");
                profile.push_str("(import \"system.sb\")\n");
                profile.push_str("(allow process*)\n");
                profile.push_str("(allow file-read*)\n");
                profile.push_str("(allow file-write*\n");
                profile.push_str(&format!("    (subpath \"{cwd_quoted}\")\n"));
                profile.push_str(&format!("    (subpath \"{tmp_quoted}\")\n"));
                profile.push_str("    (subpath \"/tmp\")\n");
                profile.push_str("    (subpath \"/private/tmp\"))\n");
            }
            TrustLevel::Permissive => {
                profile.push_str("(allow default)\n");
                profile.push_str("(import \"system.sb\")\n");
            }
        }

        // Add process-specific rules
        if self.allow_process_exec {
            profile.push_str("(allow process-exec)\n");
        }
        if self.allow_process_fork {
            profile.push_str("(allow process-fork)\n");
        }

        // Add filesystem rules based on trust level
        if self.trust_level == TrustLevel::Restricted {
            // In restricted mode, only allow explicitly listed paths
            if self.read_allowed_paths.is_empty() {
                // Add minimal read access
                profile.push_str(&format!("(allow file-read* (subpath \"{cwd_quoted}\"))\n"));
            } else {
                for path in &self.read_allowed_paths {
                    profile.push_str(&format!(
                        "(allow file-read* (subpath \"{}\"))\n",
                        path.replace('"', "\\\"")
                    ));
                }
            }

            if self.write_allowed_paths.is_empty() {
                // Add minimal write access to temp and cwd
                profile.push_str(&format!("(allow file-write* (subpath \"{tmp_quoted}\"))\n"));
                profile.push_str("(allow file-write* (subpath \"/tmp\"))\n");
                profile.push_str("(allow file-write* (subpath \"/private/tmp\"))\n");
            } else {
                for path in &self.write_allowed_paths {
                    profile.push_str(&format!(
                        "(allow file-write* (subpath \"{}\"))\n",
                        path.replace('"', "\\\"")
                    ));
                }
            }
        }

        // Network rules
        let has_network_all = self
            .network_allowlist
            .iter()
            .any(|e| matches!(e, NetworkAllowlist::All));

        if has_network_all {
            profile.push_str("(allow network*)\n");
        } else if !self.network_allowlist.is_empty() {
            // Deny all network, then allow specific entries
            profile.push_str("(deny network*)\n");
            for entry in &self.network_allowlist {
                match entry {
                    NetworkAllowlist::Tcp(ip) => {
                        profile
                            .push_str(&format!("(allow network-outbound (remote tcp \"{ip}\"))\n"));
                    }
                    NetworkAllowlist::Udp(ip) => {
                        profile
                            .push_str(&format!("(allow network-outbound (remote udp \"{ip}\"))\n"));
                    }
                    NetworkAllowlist::All => {
                        // Already handled above
                    }
                }
            }
        } else if self.trust_level != TrustLevel::Permissive {
            // Default: deny network access
            profile.push_str("(deny network*)\n");
        }

        profile
    }

    /// Build a BashSpawnPlan with these Seatbelt rules.
    pub fn build_plan(&self, cwd: &Path, shell: &str, command: &str) -> BashSpawnPlan {
        use crate::sandbox::SandboxRules;

        BashSpawnPlan {
            program: "sandbox-exec".to_string(),
            args: vec![
                "-p".to_string(),
                self.build_profile(cwd),
                shell.to_string(),
                "-c".to_string(),
                command.to_string(),
            ],
            env_overrides: Vec::new(),
            sandboxed: true,
            sandbox_wrapper: Some(format!("macos-seatbelt-{:?}", self.trust_level)),
            fallback_note: None,
            sandbox_rules: Some(SandboxRules::Default),
        }
    }
}

pub(super) fn wrapper_available() -> bool {
    static USABLE: OnceLock<bool> = OnceLock::new();
    *USABLE.get_or_init(|| {
        std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("/usr/bin/true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn build_profile(cwd: &Path) -> String {
    fn quote(path: &Path) -> String {
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    let cwd_quoted = quote(cwd);
    let tmp_quoted = quote(&std::env::temp_dir());

    format!(
        "(version 1)
(deny default)
(import \"system.sb\")
(allow process*)
(allow file-read*)
(allow file-write*
    (subpath \"{cwd_quoted}\")
    (subpath \"{tmp_quoted}\")
    (subpath \"/tmp\")
    (subpath \"/private/tmp\"))
(deny network*)"
    )
}

pub(super) fn build_plan(cwd: &Path, shell: &str, command: &str) -> BashSpawnPlan {
    // Use standard Seatbelt rules by default
    let cwd_str = cwd.display().to_string();
    SeatbeltRules::new()
        .allow_read_write(cwd_str)
        .build_plan(cwd, shell, command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seatbelt_rules_new() {
        let rules = SeatbeltRules::new();
        assert_eq!(rules.trust_level, TrustLevel::Standard);
        assert!(rules.allow_process_exec);
        assert!(rules.allow_process_fork);
    }

    #[test]
    fn test_seatbelt_rules_restricted() {
        let rules = SeatbeltRules::restricted();
        assert_eq!(rules.trust_level, TrustLevel::Restricted);
    }

    #[test]
    fn test_seatbelt_rules_permissive() {
        let rules = SeatbeltRules::permissive();
        assert_eq!(rules.trust_level, TrustLevel::Permissive);
    }

    #[test]
    fn test_seatbelt_rules_builder() {
        let rules = SeatbeltRules::new()
            .allow_read("/usr/bin")
            .allow_write("/tmp")
            .allow_read_write("/work")
            .with_process_exec(false);

        assert_eq!(rules.read_allowed_paths.len(), 2);
        assert_eq!(rules.write_allowed_paths.len(), 2);
        assert!(!rules.allow_process_exec);
    }

    #[test]
    fn test_network_allowlist() {
        let rules = SeatbeltRules::new()
            .allow_network(NetworkAllowlist::Tcp("192.168.1.1".to_string()))
            .allow_network(NetworkAllowlist::Udp("192.168.1.2".to_string()));

        assert_eq!(rules.network_allowlist.len(), 2);
    }

    #[test]
    fn test_trust_level_default() {
        assert_eq!(TrustLevel::default(), TrustLevel::Standard);
    }

    #[test]
    fn test_seatbelt_rules_build_profile() {
        let rules = SeatbeltRules::restricted();
        let cwd = Path::new("/test/path");
        let profile = rules.build_profile(cwd);

        // Check that the profile contains key elements
        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow process*)"));
    }

    #[test]
    fn test_seatbelt_rules_build_plan() {
        let rules = SeatbeltRules::new();
        let cwd = Path::new("/test/path");
        let plan = rules.build_plan(cwd, "/bin/sh", "echo test");

        assert_eq!(plan.program, "sandbox-exec");
        assert!(plan.sandboxed);
        assert!(plan.sandbox_wrapper.as_ref().unwrap().contains("seatbelt"));
        assert_eq!(plan.args.len(), 5);
    }
}
