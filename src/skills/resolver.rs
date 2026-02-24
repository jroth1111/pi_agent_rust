use std::path::Path;

pub fn is_under_path(target: &Path, root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    if target == root {
        return true;
    }
    target.starts_with(root)
}
