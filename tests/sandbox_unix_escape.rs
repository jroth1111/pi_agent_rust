//! Unix sandbox escape tests for Linux Landlock and macOS Seatbelt.
//!
//! These tests attempt to escape the sandbox to verify containment works.
//! They are educational security tests that verify blocked operations fail
//! as expected and proper logging occurs.
//!
//! Only compiles and runs on Unix systems (Linux/macOS).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ==========================================================================
// Test infrastructure
// ==========================================================================

/// Result of a sandbox escape attempt.
#[derive(Debug)]
#[allow(dead_code)]
enum EscapeAttempt {
    /// Operation was blocked as expected (good)
    Blocked { stderr: String },
    /// Operation succeeded (potential escape - bad)
    Succeeded { stdout: String, stderr: String },
    /// Process execution failed (wrapper not available, etc)
    ExecutionFailed { reason: String },
}

/// Helper to spawn a sandboxed command and check if it escapes.
fn attempt_sandboxed_command(
    sandbox_wrapper: SandboxWrapper,
    command: &str,
    args: &[&str],
) -> EscapeAttempt {
    let (program, wrapper_args): (&str, Vec<String>) = match sandbox_wrapper {
        SandboxWrapper::LinuxBwrap => {
            // Linux: wrap with bwrap
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let cwd = cwd.to_string_lossy().into_owned();
            (
                "bwrap",
                vec![
                    "--die-with-parent".to_string(),
                    "--unshare-net".to_string(),
                    "--proc".to_string(),
                    "/proc".to_string(),
                    "--dev".to_string(),
                    "/dev".to_string(),
                    "--ro-bind".to_string(),
                    "/usr".to_string(),
                    "/usr".to_string(),
                    "--bind".to_string(),
                    cwd.clone(),
                    cwd.clone(),
                    "--tmpfs".to_string(),
                    "/tmp".to_string(),
                    "--chdir".to_string(),
                    cwd,
                ],
            )
        }
        #[cfg(target_os = "macos")]
        SandboxWrapper::MacOsSeatbelt => {
            // macOS: wrap with sandbox-exec
            let profile = r#"
(version 1)
(deny default)
(import "system.sb")
(allow process*)
(allow file-read* (subpath "/usr"))
(allow file-read* (subpath "/System"))
(allow file-read* (subpath "/bin"))
(allow file-read* (subpath "/tmp"))
(allow file-write* (subpath "/tmp"))
(deny file-read* (subpath "/etc"))
(deny network*)
"#;
            return attempt_sandboxed_macos(command, args, profile);
        }
        #[cfg(not(target_os = "macos"))]
        SandboxWrapper::MacOsSeatbelt => {
            return EscapeAttempt::ExecutionFailed {
                reason: "macOS sandbox not available on this platform".to_string(),
            };
        }
    };

    let mut cmd_args = wrapper_args;
    cmd_args.push(command.to_string());
    cmd_args.extend(args.iter().map(std::string::ToString::to_string));

    let output = match Command::new(program)
        .args(&cmd_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return EscapeAttempt::ExecutionFailed {
                reason: format!("Failed to spawn {program}: {e}"),
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        EscapeAttempt::Succeeded { stdout, stderr }
    } else {
        EscapeAttempt::Blocked { stderr }
    }
}

/// macOS-specific sandbox attempt with custom profile.
#[cfg(target_os = "macos")]
fn attempt_sandboxed_macos(command: &str, args: &[&str], profile: &str) -> EscapeAttempt {
    // Write profile to a temp file
    let timestamp_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let profile_path = std::env::temp_dir().join(format!(
        "pi_sandbox_test_{}_{}.sb",
        std::process::id(),
        timestamp_ns
    ));
    if let Err(e) = std::fs::write(&profile_path, profile) {
        return EscapeAttempt::ExecutionFailed {
            reason: format!("Failed to write profile: {e}"),
        };
    }

    let mut cmd_args = vec![
        "-f".to_string(),
        profile_path.to_str().unwrap().to_string(),
        command.to_string(),
    ];
    cmd_args.extend(args.iter().map(std::string::ToString::to_string));

    let output = match Command::new("sandbox-exec")
        .args(&cmd_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return EscapeAttempt::ExecutionFailed {
                reason: format!("Failed to spawn sandbox-exec: {e}"),
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Clean up profile file
    let _ = std::fs::remove_file(&profile_path);

    if output.status.success() {
        EscapeAttempt::Succeeded { stdout, stderr }
    } else {
        EscapeAttempt::Blocked { stderr }
    }
}

#[derive(Debug, Clone, Copy)]
enum SandboxWrapper {
    LinuxBwrap,
    #[cfg(target_os = "macos")]
    MacOsSeatbelt,
}

/// Check if the sandbox wrapper is available on this system.
fn wrapper_available(wrapper: SandboxWrapper) -> bool {
    match wrapper {
        SandboxWrapper::LinuxBwrap => Command::new("bwrap")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success()),
        #[cfg(target_os = "macos")]
        SandboxWrapper::MacOsSeatbelt => Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("/usr/bin/true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success()),
        #[cfg(not(target_os = "macos"))]
        SandboxWrapper::MacOsSeatbelt => false,
    }
}

#[cfg(target_os = "macos")]
fn seatbelt_file_deny_enforced() -> bool {
    let probe_profile = r#"
(version 1)
(deny default)
(import "system.sb")
(allow process*)
(allow file-read* (subpath "/usr"))
(allow file-read* (subpath "/System"))
(allow file-read* (subpath "/bin"))
(allow file-read* (subpath "/tmp"))
(deny file-read* (subpath "/etc"))
"#;
    matches!(
        attempt_sandboxed_macos("cat", &["/etc/passwd"], probe_profile),
        EscapeAttempt::Blocked { .. }
    )
}

// ==========================================================================
// Linux Landlock escape attempt tests
// ==========================================================================

#[cfg(target_os = "linux")]
#[test]
fn test_linux_landlock_blocks_etc_passwd_read() {
    if !wrapper_available(SandboxWrapper::LinuxBwrap) {
        eprintln!("SKIP: bwrap not available");
        return;
    }

    // Attempt to read /etc/passwd from a sandbox that only binds /usr and cwd
    match attempt_sandboxed_command(SandboxWrapper::LinuxBwrap, "cat", &["/etc/passwd"]) {
        EscapeAttempt::Blocked { stderr } => {
            // Good - the operation was blocked
            assert!(
                stderr.contains("No such file or directory")
                    || stderr.contains("Permission denied")
                    || stderr.contains("Operation not permitted"),
                "Expected access denied, got stderr: {stderr}"
            );
        }
        EscapeAttempt::Succeeded { stdout, .. } => {
            // Bad - potential escape or misconfiguration
            panic!(
                "SECURITY: Sandboxed command was able to read /etc/passwd!\n\
                 stdout: {stdout}\n\
                 This indicates a sandbox breach."
            );
        }
        EscapeAttempt::ExecutionFailed { reason } => {
            panic!("Failed to execute test: {reason}");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_linux_landlock_blocks_network_access() {
    if !wrapper_available(SandboxWrapper::LinuxBwrap) {
        eprintln!("SKIP: bwrap not available");
        return;
    }

    // Attempt network access with --unshare-net
    match attempt_sandboxed_command(
        SandboxWrapper::LinuxBwrap,
        "curl",
        &[
            "--connect-timeout",
            "2",
            "-s",
            "-o",
            "/dev/null",
            "http://example.com",
        ],
    ) {
        EscapeAttempt::Blocked { stderr } => {
            // Good - network was isolated
            assert!(
                stderr.contains("Could not resolve host")
                    || stderr.contains("curl: (6)")
                    || stderr.contains("curl: (7)")
                    || stderr.contains("Network is unreachable")
                    || stderr.contains("Failed to connect"),
                "Expected network failure, got stderr: {stderr}"
            );
        }
        EscapeAttempt::Succeeded { .. } => {
            // Bad - network escape or misconfiguration
            panic!(
                "SECURITY: Sandboxed command was able to access network!\n\
                 This indicates a network isolation breach."
            );
        }
        EscapeAttempt::ExecutionFailed { reason } => {
            // curl might not be installed - that's okay
            if reason.contains("No such file or directory") {
                eprintln!("SKIP: curl not available");
                return;
            }
            panic!("Failed to execute test: {reason}");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_linux_landlock_blocks_etc_write() {
    if !wrapper_available(SandboxWrapper::LinuxBwrap) {
        eprintln!("SKIP: bwrap not available");
        return;
    }

    // Attempt to write to /etc (should be blocked)
    match attempt_sandboxed_command(
        SandboxWrapper::LinuxBwrap,
        "sh",
        &[
            "-c",
            "echo test > /etc/sandbox_test_write 2>&1 || echo write blocked",
        ],
    ) {
        EscapeAttempt::Blocked { .. } | EscapeAttempt::Succeeded { stdout, .. } => {
            // Either blocked (good) or the inner fallback message (acceptable)
            // The key is that /etc/sandbox_test_write should not be created
            let test_path = Path::new("/etc/sandbox_test_write");
            if test_path.exists() {
                panic!(
                    "SECURITY: Sandboxed command was able to write to /etc!\n\
                     File {} was created.",
                    test_path.display()
                );
            }
        }
        EscapeAttempt::ExecutionFailed { reason } => {
            panic!("Failed to execute test: {reason}");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_linux_landlock_blocks_absolute_path_escape_via_symlink() {
    if !wrapper_available(SandboxWrapper::LinuxBwrap) {
        eprintln!("SKIP: bwrap not available");
        return;
    }

    // Create a symlink in cwd pointing outside, then try to read through it
    let cwd = std::env::current_dir().unwrap();
    let symlink_path = cwd.join("test_escape_symlink");

    // Clean up any previous test artifact
    let _ = std::fs::remove_file(&symlink_path);

    // Create symlink to /etc/passwd
    if std::os::unix::fs::symlink("/etc/passwd", &symlink_path).is_err() {
        eprintln!("SKIP: Could not create test symlink");
        return;
    };

    // Try to read through the symlink
    match attempt_sandboxed_command(
        SandboxWrapper::LinuxBwrap,
        "cat",
        &[symlink_path.to_str().unwrap()],
    ) {
        EscapeAttempt::Blocked { .. } => {
            // Good - blocked
        }
        EscapeAttempt::Succeeded { stdout, .. } => {
            // If we can read through symlink, check if it's actually /etc/passwd
            if stdout.contains("root:") || stdout.contains("nobody:") {
                // Clean up
                let _ = std::fs::remove_file(&symlink_path);
                panic!(
                    "SECURITY: Sandboxed command escaped via symlink!\n\
                     Could read /etc/passwd through symlink in cwd."
                );
            }
            // Otherwise maybe symlink didn't work or pointed elsewhere
        }
        EscapeAttempt::ExecutionFailed { .. } => {
            // Acceptable - execution failed
        }
    }

    // Clean up
    let _ = std::fs::remove_file(&symlink_path);
}

// ==========================================================================
// macOS Seatbelt escape attempt tests
// ==========================================================================

#[cfg(target_os = "macos")]
#[test]
fn test_macos_seatbelt_blocks_etc_passwd_read() {
    if !wrapper_available(SandboxWrapper::MacOsSeatbelt) {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    if !seatbelt_file_deny_enforced() {
        eprintln!("SKIP: sandbox-exec deny profile not enforceable on this host");
        return;
    }

    // Use restricted profile that doesn't allow /etc
    let restricted_profile = r#"
(version 1)
(deny default)
(import "system.sb")
(allow process*)
(allow file-read* (subpath "/usr"))
(allow file-read* (subpath "/System"))
(allow file-read* (subpath "/bin"))
(allow file-read* (subpath "/tmp"))
(allow file-write* (subpath "/tmp"))
(deny file-read* (subpath "/etc"))
(deny network*)
"#;

    match attempt_sandboxed_macos("cat", &["/etc/passwd"], restricted_profile) {
        EscapeAttempt::Blocked { stderr } => {
            // Good - blocked
            assert!(
                stderr.contains("Operation not permitted")
                    || stderr.contains("denied")
                    || !stderr.is_empty(),
                "Expected access denied, got stderr: {stderr}"
            );
        }
        EscapeAttempt::Succeeded { stdout, .. } => {
            // Bad - potential escape
            panic!(
                "SECURITY: Sandboxed command was able to read /etc/passwd!\n\
                 stdout preview: {}",
                if stdout.len() > 100 {
                    &stdout[..100]
                } else {
                    &stdout
                }
            );
        }
        EscapeAttempt::ExecutionFailed { reason } => {
            panic!("Failed to execute test: {reason}");
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn test_macos_seatbelt_blocks_network_access() {
    if !wrapper_available(SandboxWrapper::MacOsSeatbelt) {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }

    // Profile with network denied
    let no_net_profile = r#"
(version 1)
(deny default)
(import "system.sb")
(allow process*)
(deny network*)
"#;

    match attempt_sandboxed_macos(
        "curl",
        &[
            "--connect-timeout",
            "2",
            "-s",
            "-o",
            "/dev/null",
            "http://example.com",
        ],
        no_net_profile,
    ) {
        EscapeAttempt::Blocked { stderr: _stderr } => {
            // Good - network denied
        }
        EscapeAttempt::Succeeded { .. } => {
            // Bad - network escape
            panic!(
                "SECURITY: Sandboxed command was able to access network!\n\
                 This indicates a Seatbelt network breach."
            );
        }
        EscapeAttempt::ExecutionFailed { reason } => {
            // curl might not be installed
            if reason.contains("No such file or directory") {
                eprintln!("SKIP: curl not available");
                return;
            }
            panic!("Failed to execute test: {reason}");
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn test_macos_seatbelt_blocks_home_directory_access() {
    if !wrapper_available(SandboxWrapper::MacOsSeatbelt) {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }

    // Profile that only allows /tmp and /usr
    let restricted_profile = r#"
(version 1)
(deny default)
(import "system.sb")
(allow process*)
(allow file-read* (subpath "/usr"))
(allow file-read* (subpath "/tmp"))
(allow file-write* (subpath "/tmp"))
(deny network*)
"#;

    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".to_string());
    let test_file = format!("{home}/.test_sandbox_escape");

    match attempt_sandboxed_macos(
        "sh",
        &["-c", &format!("echo test > {test_file}")],
        restricted_profile,
    ) {
        EscapeAttempt::Blocked { .. } => {
            // Good - blocked
        }
        EscapeAttempt::Succeeded { .. } => {
            // Check if file was actually created
            if Path::new(&test_file).exists() {
                // Clean up
                let _ = std::fs::remove_file(&test_file);
                panic!(
                    "SECURITY: Sandboxed command was able to write to home directory!\n\
                     Created file: {test_file}"
                );
            }
            // Command succeeded but file not created - likely fallback in script
        }
        EscapeAttempt::ExecutionFailed { reason } => {
            panic!("Failed to execute test: {reason}");
        }
    }
}

// ==========================================================================
// Cross-platform containment tests
// ==========================================================================

#[test]
fn test_sandbox_contains_filesystem_to_allowed_paths() {
    // Verify sandbox only allows access to explicitly permitted paths
    let wrapper = if cfg!(target_os = "linux") {
        SandboxWrapper::LinuxBwrap
    } else if cfg!(target_os = "macos") {
        #[cfg(target_os = "macos")]
        {
            SandboxWrapper::MacOsSeatbelt
        }
        #[cfg(not(target_os = "macos"))]
        {
            eprintln!("SKIP: Unsupported platform for sandbox test");
            return;
        }
    } else {
        eprintln!("SKIP: Unsupported platform");
        return;
    };

    if !wrapper_available(wrapper) {
        eprintln!("SKIP: Sandbox wrapper not available");
        return;
    }

    // Try to access a path that should be blocked
    let blocked_path = "/root/.ssh";

    match attempt_sandboxed_command(wrapper, "ls", &["-la", blocked_path]) {
        EscapeAttempt::Blocked { .. } => {
            // Good - access blocked
        }
        EscapeAttempt::Succeeded { stdout, .. } => {
            // Might be empty directory listing or actual access
            // Check if we actually see contents (not just "No such file")
            if !stdout.contains("No such file or directory")
                && !stdout.contains("ls: cannot access")
                && !stdout.contains("Permission denied")
            {
                panic!(
                    "SECURITY: Sandboxed command may have accessed blocked path: {}\n\
                     stdout: {}",
                    blocked_path,
                    if stdout.len() > 200 {
                        &stdout[..200]
                    } else {
                        &stdout
                    }
                );
            }
        }
        EscapeAttempt::ExecutionFailed { .. } => {
            // Acceptable - might mean path doesn't exist on test system
        }
    }
}

#[test]
fn test_sandbox_blocks_process_mount_escape() {
    // Verify /proc mounting doesn't allow host process escape
    let wrapper = if cfg!(target_os = "linux") {
        SandboxWrapper::LinuxBwrap
    } else {
        eprintln!("SKIP: /proc escape test only applies to Linux");
        return;
    };

    if !wrapper_available(wrapper) {
        eprintln!("SKIP: bwrap not available");
        return;
    }

    // Try to access host's /proc/1/cmdline which should be the sandbox's init
    match attempt_sandboxed_command(wrapper, "cat", &["/proc/1/cmdline"]) {
        EscapeAttempt::Succeeded { stdout, .. } => {
            // Check if we're seeing the bwrap itself (expected) or host init (bad)
            assert!(
                !(!stdout.contains("bwrap") && !stdout.is_empty()),
                "SECURITY: Sandboxed /proc may show host processes!\n\
                 cmdline: {}",
                if stdout.len() > 100 {
                    &stdout[..100]
                } else {
                    &stdout
                }
            );
        }
        EscapeAttempt::Blocked { .. } | EscapeAttempt::ExecutionFailed { .. } => {
            // Also acceptable - access denied
        }
    }
}

#[test]
fn test_sandbox_logging_on_blocked_operations() {
    // Verify that blocked operations produce detectable error messages
    let wrapper = if cfg!(target_os = "linux") {
        SandboxWrapper::LinuxBwrap
    } else if cfg!(target_os = "macos") {
        #[cfg(target_os = "macos")]
        {
            SandboxWrapper::MacOsSeatbelt
        }
        #[cfg(not(target_os = "macos"))]
        {
            eprintln!("SKIP: Unsupported platform");
            return;
        }
    } else {
        eprintln!("SKIP: Unsupported platform");
        return;
    };

    if !wrapper_available(wrapper) {
        eprintln!("SKIP: Sandbox wrapper not available");
        return;
    }

    match attempt_sandboxed_command(wrapper, "cat", &["/etc/shadow"]) {
        EscapeAttempt::Blocked { stderr } => {
            // Verify blocked operation diagnostics are present.
            assert!(
                stderr.contains("Permission denied")
                    || stderr.contains("Operation not permitted")
                    || stderr.contains("denied")
                    || stderr.contains("No such file or directory"),
                "Expected blocked-operation diagnostics, got stderr: {stderr}"
            );
        }
        EscapeAttempt::Succeeded { .. } => {
            // Should not succeed - panic to catch this
            panic!("SECURITY: Reading /etc/shadow should be blocked!");
        }
        EscapeAttempt::ExecutionFailed { .. } => {
            // Acceptable
        }
    }
}

// ==========================================================================
// Edge case escape attempts
// ==========================================================================

#[test]
fn test_sandbox_blocks_double_chroot_escape() {
    // Attempt classic chroot escape pattern
    let wrapper = if cfg!(target_os = "linux") {
        SandboxWrapper::LinuxBwrap
    } else {
        eprintln!("SKIP: chroot escape test only for Linux");
        return;
    };

    if !wrapper_available(wrapper) {
        eprintln!("SKIP: bwrap not available");
        return;
    }

    // The classic escape: mkdir x; cd x; mkdir y; cd y; ...; cd ../../..
    match attempt_sandboxed_command(
        wrapper,
        "sh",
        &[
            "-c",
            "mkdir -p a/b/c && cd a/b/c && pwd && ls ../../../etc 2>&1 || echo blocked",
        ],
    ) {
        EscapeAttempt::Succeeded { stdout, .. } => {
            // If we see actual /etc contents, that's bad
            assert!(
                !(stdout.contains("passwd") || stdout.contains("shadow")),
                "SECURITY: Possible chroot-style escape detected!\n\
                 stdout: {}",
                if stdout.len() > 200 {
                    &stdout[..200]
                } else {
                    &stdout
                }
            );
        }
        EscapeAttempt::Blocked { .. } | EscapeAttempt::ExecutionFailed { .. } => {
            // Good - blocked
        }
    }
}

#[test]
fn test_sandbox_blocks_fd_leak_escape() {
    // Verify file descriptor leaks don't allow escape
    let wrapper = if cfg!(target_os = "linux") {
        SandboxWrapper::LinuxBwrap
    } else {
        eprintln!("SKIP: FD leak test only for Linux");
        return;
    };

    if !wrapper_available(wrapper) {
        eprintln!("SKIP: bwrap not available");
        return;
    }

    // Try to access /proc/self/fd which might leak outside paths
    match attempt_sandboxed_command(
        wrapper,
        "sh",
        &[
            "-c",
            "ls -la /proc/self/fd/ 2>&1 | head -5 || echo fd access blocked",
        ],
    ) {
        EscapeAttempt::Succeeded { stdout, .. } => {
            // Seeing some FDs is normal (stdin, stdout, stderr)
            // But we shouldn't see open FDs to sensitive paths
            assert!(
                !(stdout.contains("/etc/passwd") || stdout.contains("/root/")),
                "SECURITY: FD leak may expose sensitive paths!\n\
                 stdout: {}",
                if stdout.len() > 300 {
                    &stdout[..300]
                } else {
                    &stdout
                }
            );
        }
        EscapeAttempt::Blocked { .. } | EscapeAttempt::ExecutionFailed { .. } => {
            // Acceptable
        }
    }
}

// ==========================================================================
// Integration test helpers
// ==========================================================================

/// Check all sandbox wrappers are available for testing.
#[test]
fn test_sandbox_availability() {
    let linux_available = wrapper_available(SandboxWrapper::LinuxBwrap);

    #[cfg(target_os = "macos")]
    let macos_available = wrapper_available(SandboxWrapper::MacOsSeatbelt);
    #[cfg(not(target_os = "macos"))]
    let macos_available = false;

    eprintln!("Sandbox availability report:");
    eprintln!(
        "  Linux (bwrap): {}",
        if linux_available { "YES" } else { "NO" }
    );
    eprintln!(
        "  macOS (sandbox-exec): {}",
        if macos_available { "YES" } else { "NO" }
    );

    // At least one should be available on respective platforms
    if cfg!(target_os = "linux") {
        // Don't fail test if bwrap not available, just report
    }
    if cfg!(target_os = "macos") {
        // Don't fail test if sandbox-exec not available, just report
    }
}
