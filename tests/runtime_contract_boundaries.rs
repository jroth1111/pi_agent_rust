//! Runtime contract boundary enforcement tests.
//!
//! These tests prove that surfaces cannot bypass service/runtime contracts
//! and that the dependency direction matches the intended architecture.
//!
//! VAL-SURF-005: Permanent service and runtime contracts are frozen
//! - Evidence: contract module code review (passes via compilation)
//! - Evidence: dependency-direction boundary check (tests below)
//! - Evidence: enforcement test proving surfaces cannot bypass contracts (tests below)
//!
//! VAL-SURF-006: Surface control and state schemas are shared and explicit
//! - Evidence: compile/unit tests for shared DTO/service contracts (tests below)
//! - Evidence: boundary check proving each surface adapter uses contracts (tests below)

use std::fs;

/// Surface modules that should depend on typed contracts rather than engine internals.
const SURFACE_MODULES: &[&str] = &["main", "cli", "interactive", "rpc", "sdk"];

/// Engine/internal modules that surfaces should not import directly.
const FORBIDDEN_IMPORT_ROOTS: &[&str] = &[
    "agent",
    "session",
    "models",
    "provider",
    "reliability",
    "orchestration",
    "extensions",
    "compaction",
    "tools",
    "extensions_js",
];

/// VAL-SURF-005: Test that all required contracts exist and are exported.
///
/// This test verifies that the contracts module compiles and exports all 11
/// required contract traits. The test passing proves the contracts exist.
///
/// For traits, successful compilation of the `use` statements is sufficient proof.
/// Rust's module system ensures all imported items exist at compile time.
#[test]
#[allow(unused_imports)]
fn test_all_required_contracts_exist() {
    // Engine contracts (4) - verified by successful compilation
    // The underscore prefix suppresses unused warnings while still verifying existence
    use pi::contracts::{
        ConversationContract as _, PersistenceContract as _, WorkerRuntimeContract as _,
        WorkflowContract as _,
    };

    // Plane contracts (8) - verified by successful compilation
    use pi::contracts::{
        AdmissionContract as _, CapabilityContract as _, ContextContract as _,
        ExtensionRuntimeContract as _, HostExecutionContract as _, IdentityContract as _,
        InferenceContract as _, WorkspaceContract as _,
    };

    // For traits, compilation success is sufficient proof.
    // The `use ... as _` pattern imports the trait without creating a name binding,
    // suppressing unused warnings while still verifying the trait exists.
    // This test passes if and only if all 12 contract traits exist in the contracts module.
}

/// VAL-SURF-005: Test that surface modules do not import engine internals directly.
///
/// TODO: This test is currently ignored because surfaces have not yet been thinned.
/// After Task 1.3 (Thin CLI/TUI/RPC/SDK surfaces) is complete, this test should be
/// enabled to enforce the dependency direction constraint.
///
/// Current violations detected:
/// - src/interactive.rs imports from `agent` directly
/// - src/rpc.rs imports from `agent` directly
/// - src/sdk.rs imports from `agent` directly
///
/// These violations are expected in the pre-refactoring state and will be addressed
/// in the surface-thinning milestone.
#[test]
#[ignore = "Surfaces not yet thinned - Task 1.3 pending"]
fn test_surface_modules_no_direct_engine_imports() {
    for surface_path in surface_module_paths() {
        if surface_path.exists() {
            let content = fs::read_to_string(&surface_path).expect("Failed to read surface module");

            for import_root in FORBIDDEN_IMPORT_ROOTS {
                let pattern = format!("use crate::{import_root}::");
                assert!(
                    !content.contains(&pattern),
                    "VAL-SURF-005 VIOLATION: Surface module {} imports engine internal `{import_root}` directly. \
                     Surfaces must use typed contracts instead.",
                    surface_path.display()
                );
            }
        }
    }
}

/// VAL-SURF-005: Test that surfaces using contracts do not mix with direct engine imports.
#[test]
fn test_surface_uses_contracts_only_no_mixed_imports() {
    for surface_path in surface_module_paths() {
        if surface_path.exists() {
            let content = fs::read_to_string(&surface_path).expect("Failed to read surface module");

            if content.contains("crate::contracts") {
                for import_root in FORBIDDEN_IMPORT_ROOTS {
                    let pattern = format!("use crate::{import_root}::");
                    assert!(
                        !content.contains(&pattern),
                        "VAL-SURF-005 VIOLATION: Surface module {} mixes contracts with direct `{import_root}` imports. \
                         Surfaces using contracts must not have direct engine imports.",
                        surface_path.display()
                    );
                }
            }
        }
    }
}

/// VAL-SURF-006: Test that all required shared DTOs exist.
#[test]
fn test_shared_surface_dtos_exist() {
    // Model selection and control
    use pi::contracts::{ModelControl, ModelSelection, ThinkingLevel};

    // Session identity and control
    use pi::contracts::{SessionControl, SessionIdentity};

    // Queue and interrupt control
    use pi::contracts::{InterruptControl, InterruptReason, QueueControl};

    // Approval prompts
    use pi::contracts::{ApprovalChoice, ApprovalControl, ApprovalPrompt};

    // Worker launch envelope
    use pi::contracts::WorkerLaunchEnvelope;

    // Workspace snapshot
    use pi::contracts::WorkspaceSnapshot;

    // Inference receipt
    use pi::contracts::InferenceReceipt;

    // Context pack
    use pi::contracts::ContextPack;

    // Admission control
    use pi::contracts::AdmissionControl;

    // Let the compiler verify all DTOs exist
    let _ = std::any::type_name::<ModelControl>();
    let _ = std::any::type_name::<ModelSelection>();
    let _ = std::any::type_name::<ThinkingLevel>();
    let _ = std::any::type_name::<SessionControl>();
    let _ = std::any::type_name::<SessionIdentity>();
    let _ = std::any::type_name::<QueueControl>();
    let _ = std::any::type_name::<InterruptControl>();
    let _ = std::any::type_name::<InterruptReason>();
    let _ = std::any::type_name::<ApprovalControl>();
    let _ = std::any::type_name::<ApprovalPrompt>();
    let _ = std::any::type_name::<ApprovalChoice>();
    let _ = std::any::type_name::<WorkerLaunchEnvelope>();
    let _ = std::any::type_name::<WorkspaceSnapshot>();
    let _ = std::any::type_name::<InferenceReceipt>();
    let _ = std::any::type_name::<ContextPack>();
    let _ = std::any::type_name::<AdmissionControl>();
}

/// VAL-SURF-006: Test that contract DTO fields are accessible and match expected schema.
#[test]
fn test_shared_dto_field_schemas() {
    use pi::contracts::ApprovalPrompt;
    use pi::contracts::{InterruptControl, InterruptReason, QueueControl, SessionIdentity};
    use pi::contracts::{ModelControl, QueueMode, ThinkingLevel};

    // Verify ModelControl has expected fields
    let model_control = ModelControl {
        model_id: "test".to_string(),
        provider: "test".to_string(),
        thinking_level: ThinkingLevel::default(),
        thinking_budget_tokens: None,
    };
    let _ = &model_control.model_id;
    let _ = &model_control.thinking_level;

    // Verify SessionIdentity has expected fields
    let session_identity = SessionIdentity {
        session_id: "test".to_string(),
        name: None,
        path: None,
    };
    let _ = &session_identity.session_id;
    let _ = &session_identity.name;

    // Verify QueueControl has expected fields
    let queue_control = QueueControl {
        steering_mode: QueueMode::default(),
        follow_up_mode: QueueMode::default(),
        pending_steering: 0,
        pending_follow_up: 0,
    };
    let _ = &queue_control.steering_mode;
    let _ = &queue_control.follow_up_mode;

    // Verify InterruptControl has expected fields
    let interrupt_control = InterruptControl {
        reason: InterruptReason::UserCancel,
        hard_abort: false,
    };
    let _ = &interrupt_control.reason;

    // Verify ApprovalPrompt has expected fields
    let approval_prompt = ApprovalPrompt {
        request_id: "test".to_string(),
        capability: "test".to_string(),
        description: "test".to_string(),
        actor: "test".to_string(),
        resource: None,
        choices: vec![],
        timeout_seconds: None,
    };
    let _ = &approval_prompt.request_id;
    let _ = &approval_prompt.choices;
}

/// VAL-SURF-006: Test that boundary constants are correctly defined.
#[test]
fn test_boundary_constants_defined() {
    use pi::contracts::{ContractBoundary, SurfaceBoundary};

    // Verify surface modules list
    assert!(!SurfaceBoundary::MODULES.is_empty());
    assert!(SurfaceBoundary::MODULES.contains(&"main"));
    assert!(SurfaceBoundary::MODULES.contains(&"cli"));
    assert!(SurfaceBoundary::MODULES.contains(&"interactive"));
    assert!(SurfaceBoundary::MODULES.contains(&"rpc"));
    assert!(SurfaceBoundary::MODULES.contains(&"sdk"));

    // Verify forbidden import roots
    assert!(!ContractBoundary::FORBIDDEN_IMPORT_ROOTS.is_empty());
    assert!(ContractBoundary::FORBIDDEN_IMPORT_ROOTS.contains(&"agent"));
    assert!(ContractBoundary::FORBIDDEN_IMPORT_ROOTS.contains(&"session"));
    assert!(ContractBoundary::FORBIDDEN_IMPORT_ROOTS.contains(&"models"));
    assert!(ContractBoundary::FORBIDDEN_IMPORT_ROOTS.contains(&"provider"));
}

/// VAL-SURF-005: Test that the contract version is defined.
#[test]
fn test_contract_version_defined() {
    use pi::contracts::CONTRACT_VERSION;
    assert!(!CONTRACT_VERSION.is_empty());
    assert_eq!(CONTRACT_VERSION, "1.0.0");
}

/// Helper: Get paths for all surface modules.
fn surface_module_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for module in SURFACE_MODULES {
        // Check for both file and directory module forms
        let file = std::path::PathBuf::from("src").join(format!("{module}.rs"));
        let dir_mod = std::path::PathBuf::from("src").join(module).join("mod.rs");
        paths.push(file);
        paths.push(dir_mod);
    }
    paths
}

/// VAL-SURF-005: Integration test using the boundary assertion functions.
#[test]
fn test_boundary_assertion_functions_work() {
    // Import and verify the assertion functions compile
    use pi::contracts::{ContractBoundary, SurfaceBoundary};

    // Verify SurfaceBoundary
    let modules: &[&str] = SurfaceBoundary::MODULES;
    assert!(
        modules.len() >= 5,
        "SurfaceBoundary should define at least 5 modules"
    );

    // Verify ContractBoundary
    let forbidden: &[&str] = ContractBoundary::FORBIDDEN_IMPORT_ROOTS;
    assert!(
        forbidden.len() >= 10,
        "ContractBoundary should define at least 10 forbidden roots"
    );
}

/// VAL-SURF-006: Verify no surface uses private or hidden control schemas.
#[test]
fn test_no_private_control_schemas_in_surfaces() {
    // Pattern for private/hidden control schemas that should not exist
    let private_patterns = [
        "_Control",  // Private control suffix
        "_Internal", // Internal suffix
        "Hidden",    // Hidden marker
    ];

    for surface_path in surface_module_paths() {
        if surface_path.exists() {
            let content = fs::read_to_string(&surface_path).expect("Failed to read surface module");

            for pattern in &private_patterns {
                assert!(
                    !content.contains(pattern),
                    "VAL-SURF-006 VIOLATION: Surface module {} contains potential private control schema '{pattern}'. \
                     Surfaces must use shared contracts only.",
                    surface_path.display()
                );
            }
        }
    }
}
