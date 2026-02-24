//! Language-server backend registry helpers.

use std::path::Path;

pub fn rust_backend_available(cwd: &Path) -> bool {
    cwd.join("Cargo.toml").exists()
}
