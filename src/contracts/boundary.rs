//! Contract-boundary metadata and inspection helpers.

/// Surface modules that should depend on typed contracts rather than engine
/// internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceBoundary;

impl SurfaceBoundary {
    pub const MODULES: &'static [&'static str] = &["main", "cli", "interactive", "rpc", "sdk"];
}

/// Engine/internal modules that surfaces should not import directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractBoundary;

impl ContractBoundary {
    pub const FORBIDDEN_IMPORT_ROOTS: &'static [&'static str] = &[
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
}

/// Assert that surface modules do not import engine internals directly.
pub fn assert_surface_boundary() {
    assert_no_direct_engine_imports();
}

pub fn assert_no_direct_engine_imports() {
    use std::fs;

    for surface_path in surface_module_paths() {
        let Ok(content) = fs::read_to_string(&surface_path) else {
            continue;
        };

        for import_root in ContractBoundary::FORBIDDEN_IMPORT_ROOTS {
            let pattern = format!("use crate::{import_root}::");
            assert!(
                !content.contains(&pattern),
                "surface module {} imports engine internal `{import_root}` directly",
                surface_path.display()
            );
        }
    }
}

/// Assert that surfaces which opt into contracts do so without keeping direct
/// engine imports alongside them.
pub fn assert_surface_uses_contracts_only() {
    use std::fs;

    for surface_path in surface_module_paths() {
        let Ok(content) = fs::read_to_string(&surface_path) else {
            continue;
        };
        if content.contains("crate::contracts") {
            for import_root in ContractBoundary::FORBIDDEN_IMPORT_ROOTS {
                let pattern = format!("use crate::{import_root}::");
                assert!(
                    !content.contains(&pattern),
                    "surface module {} mixes contracts with direct `{import_root}` imports",
                    surface_path.display()
                );
            }
        }
    }
}

fn surface_module_paths() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;

    let mut paths = Vec::new();
    for module in SurfaceBoundary::MODULES {
        let file = PathBuf::from("src").join(format!("{module}.rs"));
        if file.exists() {
            paths.push(file);
        }
        let module_dir = PathBuf::from("src").join(module).join("mod.rs");
        if module_dir.exists() {
            paths.push(module_dir);
        }
    }
    paths
}
