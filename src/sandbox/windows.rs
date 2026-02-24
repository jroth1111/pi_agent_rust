use std::path::Path;

use super::BashSpawnPlan;

pub(super) const fn wrapper_available() -> bool {
    false
}

#[allow(dead_code)]
pub(super) fn build_plan(_cwd: &Path, shell: &str, command: &str) -> BashSpawnPlan {
    BashSpawnPlan::direct(
        shell,
        command,
        Some(
            "Windows OS-level shell sandbox wrapper is not implemented yet; command ran without OS wrapper."
                .to_string(),
        ),
    )
}
