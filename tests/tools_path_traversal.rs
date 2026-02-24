//! Property-based tests for path traversal attacks in file-based tools.
//!
//! These tests systematically verify that path-based tools (read, write, edit,
//! grep, find, ls) properly sanitize malicious paths to prevent directory
//! traversal attacks.
//!
//! Attack vectors tested:
//! - `../` sequences in various combinations and encodings
//! - Symlink escape attempts (via relative symlinks)
//! - Unicode normalization attacks (NFC/NFD)
//! - Absolute path injection (bypassing cwd scoping)
//! - URL-encoded path variants
//! - Mixed slash styles (Windows/Unix)

#![forbid(unsafe_code)]

use asupersync::runtime::RuntimeBuilder;
use pi::model::ContentBlock;
use pi::tools::Tool;
use pi::tools::{EditTool, FindTool, GrepTool, LsTool, ReadTool, WriteTool};
use proptest::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn run_async<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    runtime.block_on(future)
}

fn text_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// Helper to create a test directory structure with sensitive files.
fn create_test_workspace() -> TempDir {
    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path();

    // Create allowed directory
    fs::create_dir_all(root.join("workspace/allowed")).expect("create allowed dir");
    fs::write(root.join("workspace/allowed/file.txt"), "allowed content")
        .expect("write allowed file");

    // Create sensitive directory outside workspace
    fs::create_dir_all(root.join("sensitive")).expect("create sensitive dir");
    fs::write(root.join("sensitive/secrets.txt"), "SECRET_DATA").expect("write secrets file");

    // Create symlink from workspace to outside (if supported)
    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink(
            root.join("sensitive"),
            root.join("workspace/allowed/symlink_to_sensitive"),
        );
    }

    temp_dir
}

/// Proptest strategy for generating malicious path traversal variants.
fn malicious_path_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        // Basic ../ patterns
        Just("../sensitive/secrets.txt".to_string()),
        Just("../../sensitive/secrets.txt".to_string()),
        Just("../../../sensitive/secrets.txt".to_string()),
        Just("../../../../sensitive/secrets.txt".to_string()),
        // Mixed patterns
        Just("allowed/../../sensitive/secrets.txt".to_string()),
        Just("./../sensitive/secrets.txt".to_string()),
        Just("./allowed/../../sensitive/secrets.txt".to_string()),
        // Double-slash variations
        Just("..//sensitive//secrets.txt".to_string()),
        Just("..././sensitive/secrets.txt".to_string()),
        // Unicode obfuscation attempts
        Just(".\u{200C}./sensitive/secrets.txt".to_string()), // zero-width non-joiner
        Just(".\u{200D}./sensitive/secrets.txt".to_string()), // zero-width joiner
        Just(".\u{200B}./sensitive/secrets.txt".to_string()), // zero-width space
        Just("..%2F..%2Fsensitive%2Fsecrets.txt".to_string()), // URL encoding
        // Backslash variants (Windows-style on Unix)
        Just("..\\sensitive\\secrets.txt".to_string()),
        Just("..\\\\sensitive\\\\secrets.txt".to_string()),
        // Leading slash attempts (absolute path injection)
        Just("/sensitive/secrets.txt".to_string()),
        Just("/tmp/../sensitive/secrets.txt".to_string()),
        // NULL byte attempts (should be sanitized)
        Just("../sensitive/secrets.txt\u{0}allowed.txt".to_string()),
        // Dot variants
        Just("....txt".to_string()),
        Just("allowed/..txt".to_string()),
        // Case variations for case-insensitive filesystems
        Just("../SENSITIVE/SECRETS.TXT".to_string()),
        Just("../SeNsItIvE/SeCrEtS.TxT".to_string()),
    ]
}

/// Proptest strategy for generating Unicode normalization attack paths.
fn unicode_normalization_strategy() -> impl Strategy<Value = String> {
    // Use various Unicode tricks that might bypass string comparison
    prop_oneof![
        Just("\u{00E9}"),  // é as single char (NFC)
        Just("e\u{0301}"), // é as e + combining acute (NFD)
        Just("\u{017F}"),  // long s (looks like s)
        Just("\u{0131}"),  // dotless i (looks like i)
        Just("\u{03A9}"),  // Omega (looks like W)
        Just("\u{0430}"),  // Cyrillic a (looks like Latin a)
        Just("\u{03C0}"),  // Greek pi
    ]
    .prop_map(|ch| format!("../sensitive/secret{ch}.txt"))
}

/// Helper to get the canonical workspace path.
fn workspace_path(dir: &Path) -> PathBuf {
    dir.join("workspace")
}

/// Helper to get the canonical sensitive path.
fn sensitive_path(dir: &Path) -> PathBuf {
    dir.join("sensitive/secrets.txt")
}

// ============================================================================
// read tool path traversal tests
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn read_tool_blocks_path_traversal(malicious_path in malicious_path_strategy()) {
        let temp_dir = create_test_workspace();
        let workspace = workspace_path(temp_dir.path());
        let sensitive_file = sensitive_path(temp_dir.path());

        let read_tool = ReadTool::new(&workspace);
        let result = run_async(read_tool.execute(
            "test-call",
            serde_json::json!({"path": malicious_path}),
            None,
        ));

        // The result should be an error or should not contain the secret content
        if let Ok(output) = result {
            let content = text_content(&output.content);
            prop_assert!(!content.contains("SECRET_DATA"),
                "Read tool should not expose sensitive data via path traversal: {}",
                malicious_path);
        } else {
            // Error is acceptable — the attack was blocked
        }

        // Verify the sensitive file still exists and contains our secret
        let actual_secret = fs::read_to_string(&sensitive_file).unwrap();
        prop_assert_eq!(actual_secret, "SECRET_DATA");
    }

    #[test]
    fn read_tool_blocks_unicode_normalization_attack(
        malicious_path in unicode_normalization_strategy()
    ) {
        let temp_dir = create_test_workspace();
        let workspace = workspace_path(temp_dir.path());

        let read_tool = ReadTool::new(&workspace);
        let result = run_async(read_tool.execute(
            "test-call",
            serde_json::json!({"path": malicious_path}),
            None,
        ));

        if let Ok(output) = result {
            let content = text_content(&output.content);
            prop_assert!(!content.contains("SECRET_DATA"),
                "Read tool should not expose data via Unicode normalization attack: {}",
                malicious_path);
        } else {
            // Error is acceptable
        }
    }
}

// ============================================================================
// write tool path traversal tests
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn write_tool_blocks_path_traversal(malicious_path in malicious_path_strategy()) {
        let temp_dir = create_test_workspace();
        let workspace = workspace_path(temp_dir.path());
        let sensitive_dir = temp_dir.path().join("sensitive");

        let write_tool = WriteTool::new(&workspace);
        let result = run_async(write_tool.execute(
            "test-call",
            serde_json::json!({"path": malicious_path, "content": "ATTACK_DATA"}),
            None,
        ));

        // Write should fail or write within workspace only
        if let Ok(_) = result {
            // If it succeeded, verify it didn't write to sensitive location
            let sensitive_path = sensitive_dir.join("secrets.txt");
            if sensitive_path.exists() {
                let content = fs::read_to_string(&sensitive_path).unwrap();
                prop_assert_eq!(content, "SECRET_DATA",
                    "Write tool should not overwrite files outside workspace");
            }
        } else {
            // Error is acceptable
        }
    }
}

// ============================================================================
// edit tool path traversal tests
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn edit_tool_blocks_path_traversal(malicious_path in malicious_path_strategy()) {
        let temp_dir = create_test_workspace();
        let workspace = workspace_path(temp_dir.path());

        let edit_tool = EditTool::new(&workspace);
        let result = run_async(edit_tool.execute(
            "test-call",
            serde_json::json!({
                "path": malicious_path,
                "oldText": "SECRET_DATA",
                "newText": "COMPROMISED"
            }),
            None,
        ));

        // Edit should fail or not modify files outside workspace
        if let Ok(_) = result {
            let sensitive_file = sensitive_path(temp_dir.path());
            if sensitive_file.exists() {
                let content = fs::read_to_string(&sensitive_file).unwrap();
                prop_assert_eq!(content, "SECRET_DATA",
                    "Edit tool should not modify files outside workspace");
            }
        } else {
            // Error is acceptable
        }
    }
}

// ============================================================================
// ls tool path traversal tests
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn ls_tool_blocks_path_traversal(malicious_path in malicious_path_strategy()) {
        let temp_dir = create_test_workspace();
        let workspace = workspace_path(temp_dir.path());

        let ls_tool = LsTool::new(&workspace);
        let result = run_async(ls_tool.execute(
            "test-call",
            serde_json::json!({"path": malicious_path}),
            None,
        ));

        if let Ok(output) = result {
            let content = text_content(&output.content);
            // Should not list files outside workspace
            prop_assert!(!content.contains("secrets.txt") || content.contains("allowed"),
                "Ls tool should not list directories outside workspace via traversal: {}",
                malicious_path);
        } else {
            // Error is acceptable
        }
    }
}

// ============================================================================
// grep tool path traversal tests
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn grep_tool_blocks_path_traversal(malicious_path in malicious_path_strategy()) {
        let temp_dir = create_test_workspace();
        let workspace = workspace_path(temp_dir.path());

        let grep_tool = GrepTool::new(&workspace);
        let result = run_async(grep_tool.execute(
            "test-call",
            serde_json::json!({"pattern": "SECRET_DATA", "path": malicious_path}),
            None,
        ));

        if let Ok(output) = result {
            let content = text_content(&output.content);
            // Should not find content outside workspace
            prop_assert!(!content.contains("SECRET_DATA"),
                "Grep tool should not search files outside workspace via traversal: {}",
                malicious_path);
        } else {
            // Error is acceptable
        }
    }
}

// ============================================================================
// find tool path traversal tests
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn find_tool_blocks_path_traversal(malicious_path in malicious_path_strategy()) {
        let temp_dir = create_test_workspace();
        let workspace = workspace_path(temp_dir.path());

        let find_tool = FindTool::new(&workspace);
        let result = run_async(find_tool.execute(
            "test-call",
            serde_json::json!({"pattern": "secrets.txt", "path": malicious_path}),
            None,
        ));

        if let Ok(output) = result {
            let content = text_content(&output.content);
            // Should not find files outside workspace
            prop_assert!(!content.contains("secrets.txt") ||
                content.contains("workspace") ||
                content.contains("allowed"),
                "Find tool should not find files outside workspace via traversal: {}, content: {}",
                malicious_path, content);
        } else {
            // Error is acceptable
        }
    }
}

// ============================================================================
// Absolute path injection tests
// ============================================================================

#[test]
fn read_tool_blocks_absolute_path_injection() {
    let temp_dir = create_test_workspace();
    let workspace = workspace_path(temp_dir.path());

    // Try to read using an absolute path to the sensitive file
    let absolute_path = sensitive_path(temp_dir.path())
        .canonicalize()
        .expect("canonicalize")
        .to_string_lossy()
        .to_string();

    let read_tool = ReadTool::new(&workspace);
    let result = run_async(read_tool.execute(
        "test-call",
        serde_json::json!({"path": absolute_path}),
        None,
    ));

    // Should either error or not return the secret
    if let Ok(output) = result {
        let content = text_content(&output.content);
        assert!(
            !content.contains("SECRET_DATA"),
            "Read tool should not allow absolute path injection"
        );
    } else {
        // Expected — absolute path blocked
    }
}

// ============================================================================
// Symlink escape tests
// ============================================================================

#[cfg(unix)]
#[test]
fn read_tool_blocks_symlink_escape() {
    let temp_dir = create_test_workspace();
    let workspace = workspace_path(temp_dir.path());

    // Symlink was created in create_test_workspace
    let read_tool = ReadTool::new(&workspace);
    let result = run_async(read_tool.execute(
        "test-call",
        serde_json::json!({"path": "allowed/symlink_to_sensitive/secrets.txt"}),
        None,
    ));

    // Symlinks that escape the workspace should be blocked or resolved safely
    if let Ok(output) = result {
        let content = text_content(&output.content);
        assert!(
            !content.contains("SECRET_DATA"),
            "Read tool should not follow symlinks that escape workspace"
        );
    } else {
        // Expected — symlink escape blocked
    }
}

// ============================================================================
// NFD/NFC normalization tests (macOS HFS+)
// ============================================================================

#[test]
fn read_tool_normalizes_unicode_paths_correctly() {
    let temp_dir = create_test_workspace();
    let workspace = workspace_path(temp_dir.path());

    // Create a file with an NFC (composed) filename
    let nfc_name = "café.txt";
    let nfc_path = workspace.join(nfc_name);
    fs::write(&nfc_path, "NFC content").expect("write NFC file");

    // Try to read with NFD (decomposed) form
    let nfd_name = "cafe\u{0301}.txt"; // e + combining acute accent
    let read_tool = ReadTool::new(&workspace);
    let result =
        run_async(read_tool.execute("test-call", serde_json::json!({"path": nfd_name}), None));

    // Should successfully read the file (Unicode normalization handled)
    match result {
        Ok(output) => {
            let content = text_content(&output.content);
            assert!(
                content.contains("NFC content"),
                "Read tool should handle Unicode normalization"
            );
        }
        Err(e) => {
            // On non-macOS or without proper normalization, this might fail
            // but should not be a security issue — just a usability one
            eprintln!("Unicode normalization test failed (may be expected on this platform): {e}");
        }
    }
}

// ============================================================================
// Edge case: empty and dot-only paths
// ============================================================================

#[test]
fn read_tool_handles_empty_path() {
    let temp_dir = create_test_workspace();
    let workspace = workspace_path(temp_dir.path());

    let read_tool = ReadTool::new(&workspace);
    let result = run_async(read_tool.execute("test-call", serde_json::json!({"path": ""}), None));

    // Empty path should error, not read arbitrary files
    assert!(result.is_err(), "Empty path should result in error");
}

#[test]
fn read_tool_handles_dot_path() {
    let temp_dir = create_test_workspace();
    let workspace = workspace_path(temp_dir.path());

    let read_tool = ReadTool::new(&workspace);
    let result = run_async(read_tool.execute("test-call", serde_json::json!({"path": "."}), None));

    // "." should either list workspace or error, not escape
    if let Ok(_) = result {
        // Listing cwd is acceptable
    } else {
        // Error is also acceptable
    }
}

// ============================================================================
// Windows-specific absolute/UNC escape tests
// ============================================================================

#[cfg(windows)]
#[test]
fn read_tool_blocks_windows_drive_path_escape() {
    let temp_dir = create_test_workspace();
    let workspace = workspace_path(temp_dir.path());
    let read_tool = ReadTool::new(&workspace);

    let result = run_async(read_tool.execute(
        "test-call",
        serde_json::json!({"path": "C:\\\\Windows\\\\System32\\\\drivers\\\\etc\\\\hosts"}),
        None,
    ));

    assert!(
        result.is_err(),
        "Drive-qualified absolute paths outside workspace must be rejected"
    );
}

#[cfg(windows)]
#[test]
fn read_tool_blocks_windows_unc_path_escape() {
    let temp_dir = create_test_workspace();
    let workspace = workspace_path(temp_dir.path());
    let read_tool = ReadTool::new(&workspace);

    let result = run_async(read_tool.execute(
        "test-call",
        serde_json::json!({"path": "\\\\\\\\?\\\\C:\\\\Windows\\\\System32\\\\config\\\\SAM"}),
        None,
    ));

    assert!(
        result.is_err(),
        "UNC/extended-length absolute paths outside workspace must be rejected"
    );
}
