//! Linux sandbox implementation using bubblewrap and Landlock.
//!
//! This module provides OS-level sandboxing for Linux using:
//! - **bubblewrap (bwrap)**: User-space sandbox for filesystem isolation
//! - **Landlock**: Linux kernel ABI for access control (kernel 5.13+)

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;

use super::BashSpawnPlan;

/// Filesystem access permissions for sandbox rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsAccess {
    /// Read-only access to files and directories.
    Read,
    /// Write access to files and directories.
    Write,
    /// Execute access to binaries and scripts.
    Execute,
}

impl FsAccess {
    /// Convert to bwap bind flag.
    fn as_bwrap_arg(&self) -> &'static str {
        match self {
            Self::Read => "--ro-bind",
            Self::Write => "--bind",
            Self::Execute => "--ro-bind", // Execute implies read-only bind
        }
    }
}

/// Landlock-compatible filesystem access rules.
///
/// Landlock is a Linux kernel feature (since 5.13) that provides
/// unprivileged access control. When available, we use it for
/// finer-grained access control beyond bubblewrap.
#[derive(Debug, Clone, Default)]
pub struct LandlockRules {
    /// Paths that are explicitly allowed with specific permissions.
    pub allowed_paths: Vec<(String, Vec<FsAccess>)>,
    /// Paths that are explicitly forbidden.
    pub forbidden_paths: Vec<String>,
    /// Whether to enable network isolation.
    pub network_isolated: bool,
    /// Temporary directory path (defaults to /tmp).
    pub tmp_dir: Option<String>,
}

impl LandlockRules {
    /// Create a new empty set of Landlock rules.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a path with read access.
    pub fn allow_read(mut self, path: impl Into<String>) -> Self {
        self.allowed_paths.push((path.into(), vec![FsAccess::Read]));
        self
    }

    /// Add a path with write access.
    pub fn allow_write(mut self, path: impl Into<String>) -> Self {
        self.allowed_paths
            .push((path.into(), vec![FsAccess::Write]));
        self
    }

    /// Add a path with read and write access.
    pub fn allow_read_write(mut self, path: impl Into<String>) -> Self {
        self.allowed_paths
            .push((path.into(), vec![FsAccess::Read, FsAccess::Write]));
        self
    }

    /// Add a path with execute access.
    pub fn allow_execute(mut self, path: impl Into<String>) -> Self {
        self.allowed_paths
            .push((path.into(), vec![FsAccess::Execute]));
        self
    }

    /// Add a path with custom access permissions.
    pub fn allow(mut self, path: impl Into<String>, access: Vec<FsAccess>) -> Self {
        self.allowed_paths.push((path.into(), access));
        self
    }

    /// Add a forbidden path.
    pub fn forbid(mut self, path: impl Into<String>) -> Self {
        self.forbidden_paths.push(path.into());
        self
    }

    /// Enable network isolation.
    pub fn with_network_isolation(mut self, isolated: bool) -> Self {
        self.network_isolated = isolated;
        self
    }

    /// Set custom temporary directory.
    pub fn with_tmp_dir(mut self, path: impl Into<String>) -> Self {
        self.tmp_dir = Some(path.into());
        self
    }

    /// Check if Landlock is available in the current kernel.
    pub fn is_landlock_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            // Check for Landlock support by reading kernel version
            if let Ok(uname) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
                let version = uname.trim();
                if let Some(major) = version.split('.').next() {
                    if let Ok(major_ver) = major.parse::<u32>() {
                        return major_ver >= 6; // Linux 6.x always has Landlock
                    }
                }
                // Check for 5.13+ specifically
                if let Some(minor) = version.split('.').nth(1) {
                    if let Ok(minor_ver) = minor.parse::<u32>() {
                        return minor_ver >= 13;
                    }
                }
            }
            false
        })
    }
}

/// Build a bubblewrap plan with Landlock rules.
///
/// This function constructs a bwap command with filesystem and network
/// isolation based on the provided Landlock rules.
pub fn build_plan_with_rules(
    cwd: &Path,
    shell: &str,
    command: &str,
    rules: &LandlockRules,
) -> BashSpawnPlan {
    let cwd_display = cwd.display().to_string();
    let mut args = vec!["--die-with-parent".to_string()];

    // Network isolation
    if rules.network_isolated {
        args.push("--unshare-net".to_string());
    }

    // Basic filesystem namespace
    args.extend([
        "--proc".to_string(),
        "/proc".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
    ]);

    // Track which paths we've already added
    let mut added_paths = HashSet::new();

    // Build bind mounts for allowed paths
    for (path, access_types) in &rules.allowed_paths {
        if added_paths.contains(path) {
            continue;
        }

        // Add the path with the first access type's flag
        // For multiple access types, we use write if present
        let flag = if access_types.contains(&FsAccess::Write) {
            FsAccess::Write
        } else if access_types.contains(&FsAccess::Execute) {
            FsAccess::Execute
        } else {
            FsAccess::Read
        };

        args.push(flag.as_bwrap_arg().to_string());
        args.push(path.clone());
        args.push(path.clone());
        added_paths.insert(path.clone());
    }

    // Always bind mount the working directory if not already added
    let cwd_str = cwd_display.clone();
    if !added_paths.contains(&cwd_str) {
        args.extend(["--bind".to_string(), cwd_str.clone(), cwd_str.clone()]);
        added_paths.insert(cwd_str);
    }

    // Set up tmpfs
    let tmp = rules.tmp_dir.as_deref().unwrap_or("/tmp");
    if !added_paths.contains(tmp) {
        args.extend(["--tmpfs".to_string(), tmp.to_string()]);
    }

    // Set working directory
    args.extend(["--chdir".to_string(), cwd_display.clone()]);

    // Command
    args.extend([shell.to_string(), "-c".to_string(), command.to_string()]);

    // Build environment overrides
    let mut env_overrides = vec![
        ("TMPDIR".to_string(), tmp.to_string()),
        ("TMP".to_string(), tmp.to_string()),
        ("TEMP".to_string(), tmp.to_string()),
    ];

    // Add PATH if needed
    if let Ok(path) = std::env::var("PATH") {
        env_overrides.push(("PATH".to_string(), path));
    }

    BashSpawnPlan {
        program: "bwrap".to_string(),
        args,
        env_overrides,
        sandboxed: true,
        sandbox_wrapper: Some(format!(
            "linux-bwrap{}",
            if LandlockRules::is_landlock_available() {
                "+landlock"
            } else {
                ""
            }
        )),
        fallback_note: None,
        sandbox_rules: None, // Will be set by caller
    }
}

pub(super) fn wrapper_available() -> bool {
    static USABLE: OnceLock<bool> = OnceLock::new();
    *USABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .args([
                "--die-with-parent",
                "--unshare-net",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--ro-bind",
                "/",
                "/",
                "--tmpfs",
                "/tmp",
                "--chdir",
                "/",
                "/bin/sh",
                "-c",
                "true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

pub(super) fn build_plan(cwd: &Path, shell: &str, command: &str) -> BashSpawnPlan {
    // Use default Landlock rules for standard sandboxing
    let rules = LandlockRules::new()
        .allow_read_write(cwd)
        .with_network_isolation(true);

    build_plan_with_rules(cwd, shell, command, &rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_landlock_rules_new() {
        let rules = LandlockRules::new();
        assert!(rules.allowed_paths.is_empty());
        assert!(rules.forbidden_paths.is_empty());
        assert!(rules.network_isolated);
        assert!(rules.tmp_dir.is_none());
    }

    #[test]
    fn test_landlock_rules_builder() {
        let rules = LandlockRules::new()
            .allow_read("/usr/bin")
            .allow_write("/tmp")
            .allow_execute("/bin/sh")
            .forbid("/etc/passwd");

        assert_eq!(rules.allowed_paths.len(), 3);
        assert_eq!(rules.forbidden_paths.len(), 1);
        assert!(rules.network_isolated);
    }

    #[test]
    fn test_landlock_rules_read_write() {
        let rules = LandlockRules::new().allow_read_write("/tmp/work");
        assert_eq!(rules.allowed_paths.len(), 1);
        assert_eq!(rules.allowed_paths[0].1.len(), 2);
    }

    #[test]
    fn test_landlock_rules_network_isolation() {
        let rules = LandlockRules::new().with_network_isolation(false);
        assert!(!rules.network_isolated);
    }

    #[test]
    fn test_fs_access_bwrap_arg() {
        assert_eq!(FsAccess::Read.as_bwrap_arg(), "--ro-bind");
        assert_eq!(FsAccess::Write.as_bwrap_arg(), "--bind");
        assert_eq!(FsAccess::Execute.as_bwrap_arg(), "--ro-bind");
    }
}
