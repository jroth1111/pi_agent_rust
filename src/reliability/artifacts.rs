use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactQuery {
    pub task_id: String,
    pub kind: Option<String>,
    pub limit: usize,
}

impl ArtifactQuery {
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            kind: None,
            limit: 100,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactStoreError {
    #[error("artifact not found: {0}")]
    NotFound(String),
    #[error("invalid artifact identifier: {0}")]
    InvalidId(String),
    #[error("artifact store I/O failure: {0}")]
    Io(#[from] std::io::Error),
}

pub trait ArtifactStore {
    fn put_bytes(
        &self,
        task_id: &str,
        kind: &str,
        bytes: &[u8],
    ) -> Result<String, ArtifactStoreError>;

    fn put_text(
        &self,
        task_id: &str,
        kind: &str,
        text: &str,
    ) -> Result<String, ArtifactStoreError> {
        self.put_bytes(task_id, kind, text.as_bytes())
    }

    fn load(&self, artifact_id: &str) -> Result<Vec<u8>, ArtifactStoreError>;

    fn list(&self, query: &ArtifactQuery) -> Result<Vec<String>, ArtifactStoreError>;
}

#[derive(Debug, Clone)]
pub struct FsArtifactStore {
    root: PathBuf,
}

impl FsArtifactStore {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ArtifactStoreError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn task_kind_dir(&self, task_id: &str, kind: &str) -> Result<PathBuf, ArtifactStoreError> {
        validate_path_segment(task_id, "task_id")?;
        validate_path_segment(kind, "kind")?;
        Ok(self.root.join(task_id).join(kind))
    }

    fn artifact_path(&self, artifact_id: &str) -> Result<PathBuf, ArtifactStoreError> {
        let rel = sanitize_relative_artifact_id(artifact_id)?;
        let candidate = self.root.join(rel);
        if !candidate.starts_with(&self.root) {
            return Err(ArtifactStoreError::InvalidId(artifact_id.to_string()));
        }
        Ok(candidate)
    }
}

fn validate_path_segment(value: &str, field: &str) -> Result<(), ArtifactStoreError> {
    if value.trim().is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(ArtifactStoreError::InvalidId(format!(
            "invalid {field}: {value}"
        )));
    }
    Ok(())
}

fn sanitize_relative_artifact_id(artifact_id: &str) -> Result<PathBuf, ArtifactStoreError> {
    if artifact_id.trim().is_empty() {
        return Err(ArtifactStoreError::InvalidId(artifact_id.to_string()));
    }

    let path = Path::new(artifact_id);
    if path.is_absolute() {
        return Err(ArtifactStoreError::InvalidId(artifact_id.to_string()));
    }

    let mut sanitized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(seg) => sanitized.push(seg),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(ArtifactStoreError::InvalidId(artifact_id.to_string()));
            }
        }
    }

    if sanitized.as_os_str().is_empty() {
        return Err(ArtifactStoreError::InvalidId(artifact_id.to_string()));
    }

    Ok(sanitized)
}

impl ArtifactStore for FsArtifactStore {
    fn put_bytes(
        &self,
        task_id: &str,
        kind: &str,
        bytes: &[u8],
    ) -> Result<String, ArtifactStoreError> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut digest_hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            let _ = write!(&mut digest_hex, "{byte:02x}");
        }

        let file_name = format!("{digest_hex}.bin");
        let dir = self.task_kind_dir(task_id, kind)?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(file_name);
        if !path.exists() {
            std::fs::write(&path, bytes)?;
        }

        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| ArtifactStoreError::InvalidId(path.display().to_string()))?;
        Ok(relative.to_string_lossy().to_string())
    }

    fn load(&self, artifact_id: &str) -> Result<Vec<u8>, ArtifactStoreError> {
        let path = self.artifact_path(artifact_id)?;
        if !path.exists() {
            return Err(ArtifactStoreError::NotFound(artifact_id.to_string()));
        }
        Ok(std::fs::read(path)?)
    }

    fn list(&self, query: &ArtifactQuery) -> Result<Vec<String>, ArtifactStoreError> {
        validate_path_segment(&query.task_id, "task_id")?;
        if let Some(kind) = &query.kind {
            validate_path_segment(kind, "kind")?;
        }

        let mut ids = Vec::new();
        let base = self.root.join(&query.task_id);
        if !base.exists() {
            return Ok(ids);
        }

        let mut gather_from_kind_dir = |kind_dir: PathBuf| -> Result<(), ArtifactStoreError> {
            if !kind_dir.exists() {
                return Ok(());
            }
            for entry in std::fs::read_dir(&kind_dir)? {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let rel = path
                    .strip_prefix(&self.root)
                    .map_err(|_| ArtifactStoreError::InvalidId(path.display().to_string()))?;
                ids.push(rel.to_string_lossy().to_string());
            }
            Ok(())
        };

        if let Some(kind) = &query.kind {
            gather_from_kind_dir(base.join(kind))?;
        } else {
            for kind_entry in std::fs::read_dir(&base)? {
                let kind_entry = kind_entry?;
                if kind_entry.path().is_dir() {
                    gather_from_kind_dir(kind_entry.path())?;
                }
            }
        }

        ids.sort();
        ids.reverse();
        ids.truncate(query.limit);
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fs_artifact_store_put_load_and_list() {
        let tmp = TempDir::new().expect("tempdir");
        let store = FsArtifactStore::new(tmp.path()).expect("store");

        let id_a = store
            .put_text("task-1", "verify", "stdout line")
            .expect("put a");
        let id_b = store.put_bytes("task-1", "stderr", b"oops").expect("put b");

        let loaded = store.load(&id_a).expect("load a");
        assert_eq!(loaded, b"stdout line");

        let mut query = ArtifactQuery::new("task-1");
        query.limit = 10;
        let all = store.list(&query).expect("list");
        assert_eq!(all.len(), 2);
        assert!(all.contains(&id_a));
        assert!(all.contains(&id_b));

        query.kind = Some("verify".to_string());
        let verify_only = store.list(&query).expect("list verify");
        assert_eq!(verify_only, vec![id_a]);
    }

    #[test]
    fn fs_artifact_store_ids_are_deterministic_for_same_content() {
        let tmp = TempDir::new().expect("tempdir");
        let store = FsArtifactStore::new(tmp.path()).expect("store");

        let first = store
            .put_text("task-1", "verify", "identical output")
            .expect("first write");
        let second = store
            .put_text("task-1", "verify", "identical output")
            .expect("second write");

        assert_eq!(first, second);
    }

    #[test]
    fn fs_artifact_store_returns_not_found_for_missing_id() {
        let tmp = TempDir::new().expect("tempdir");
        let store = FsArtifactStore::new(tmp.path()).expect("store");
        let err = store
            .load("task-1/verify/missing.bin")
            .expect_err("missing");
        assert!(matches!(err, ArtifactStoreError::NotFound(_)));
    }

    #[test]
    fn fs_artifact_store_rejects_invalid_task_segment() {
        let tmp = TempDir::new().expect("tempdir");
        let store = FsArtifactStore::new(tmp.path()).expect("store");
        let err = store
            .put_text("../escape", "verify", "x")
            .expect_err("must reject traversal");
        assert!(matches!(err, ArtifactStoreError::InvalidId(_)));
    }

    #[test]
    fn fs_artifact_store_rejects_traversal_artifact_id() {
        let tmp = TempDir::new().expect("tempdir");
        let store = FsArtifactStore::new(tmp.path()).expect("store");
        let err = store
            .load("../outside.bin")
            .expect_err("must reject invalid id");
        assert!(matches!(err, ArtifactStoreError::InvalidId(_)));
    }

    #[test]
    fn fs_artifact_store_list_rejects_invalid_kind() {
        let tmp = TempDir::new().expect("tempdir");
        let store = FsArtifactStore::new(tmp.path()).expect("store");
        let mut query = ArtifactQuery::new("task-1");
        query.kind = Some("../verify".to_string());
        let err = store.list(&query).expect_err("must reject invalid kind");
        assert!(matches!(err, ArtifactStoreError::InvalidId(_)));
    }
}
