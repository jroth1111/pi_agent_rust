use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const MAX_POLICY_FILES: usize = 8;
const MAX_FILE_CHARS: usize = 2_000;
const MAX_TOTAL_CHARS: usize = 6_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPolicyDigest {
    pub digest_hex: String,
    pub files: Vec<ContextFile>,
    pub rendered: String,
    pub total_source_chars: usize,
    pub truncated: bool,
}

pub fn load_project_context_files(cwd: &Path, global_dir: &Path) -> Vec<ContextFile> {
    let mut context_files = Vec::new();
    let mut seen = HashSet::new();

    if let Some(global) = load_context_file_from_dir(global_dir) {
        seen.insert(global.path.clone());
        context_files.push(global);
    }

    let mut ancestor_files = Vec::new();
    let mut current = cwd.to_path_buf();

    loop {
        if let Some(context) = load_context_file_from_dir(&current)
            && seen.insert(context.path.clone())
        {
            ancestor_files.push(context);
        }

        if !current.pop() {
            break;
        }
    }

    ancestor_files.reverse();
    context_files.extend(ancestor_files);
    context_files
}

pub fn build_repo_policy_digest(files: &[ContextFile]) -> Option<RepoPolicyDigest> {
    if files.is_empty() {
        return None;
    }

    let limited_files = files
        .iter()
        .take(MAX_POLICY_FILES)
        .cloned()
        .collect::<Vec<_>>();
    let total_source_chars = limited_files.iter().map(|file| file.content.len()).sum();

    let mut hasher = Sha256::new();
    for file in &limited_files {
        hasher.update(file.path.as_bytes());
        hasher.update([0]);
        hasher.update(file.content.as_bytes());
        hasher.update([0xff]);
    }
    let digest_hex = format!("{:x}", hasher.finalize());

    let mut rendered = String::from(
        "# Repo Policy Digest\n\nUse these project-specific instructions as binding policy. The raw policy files are loaded from disk but compacted here to bound prompt size.\n\n",
    );
    let mut remaining = MAX_TOTAL_CHARS;
    let mut truncated = files.len() > limited_files.len();

    for file in &limited_files {
        if remaining == 0 {
            truncated = true;
            break;
        }

        let title = format!("## {}\n\n", file.path);
        rendered.push_str(&title);

        let file_budget = remaining.min(MAX_FILE_CHARS);
        let snippet = clip_chars(&file.content, file_budget);
        if snippet.len() < file.content.len() {
            truncated = true;
        }
        rendered.push_str(&snippet);
        rendered.push_str("\n\n");
        remaining = remaining.saturating_sub(snippet.len());
    }

    if truncated {
        rendered.push_str(
            "[Repo policy digest was truncated to stay within the prompt budget. Prefer the most specific nested policy file when instructions appear to conflict.]\n",
        );
    }

    Some(RepoPolicyDigest {
        digest_hex,
        files: limited_files,
        rendered,
        total_source_chars,
        truncated,
    })
}

fn load_context_file_from_dir(dir: &Path) -> Option<ContextFile> {
    let candidates = ["AGENTS.md", "CLAUDE.md"];
    for filename in candidates {
        let path = dir.join(filename);
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    return Some(ContextFile {
                        path: path.display().to_string(),
                        content,
                    });
                }
                Err(err) => {
                    eprintln!("Warning: Could not read {}: {err}", path.display());
                }
            }
        }
    }
    None
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    let clipped = text.chars().take(max_chars).collect::<String>();
    if clipped.len() < text.len() {
        format!("{clipped}\n[truncated]")
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn repo_policy_digest_limits_large_files() {
        let digest = build_repo_policy_digest(&[ContextFile {
            path: "/tmp/project/AGENTS.md".to_string(),
            content: "x".repeat(MAX_FILE_CHARS + 500),
        }])
        .expect("digest should exist");

        assert!(digest.truncated);
        assert!(digest.rendered.contains("# Repo Policy Digest"));
        assert!(digest.rendered.contains("[truncated]"));
        assert!(digest.rendered.len() < MAX_TOTAL_CHARS + 500);
    }

    #[test]
    fn load_project_context_files_prefers_global_then_ancestors() {
        let root = tempdir().expect("tempdir");
        let project = root.path().join("project");
        let nested = project.join("src");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(root.path().join("AGENTS.md"), "global").expect("write global");
        std::fs::write(project.join("CLAUDE.md"), "project").expect("write project");

        let files = load_project_context_files(&nested, root.path());
        let paths = files
            .iter()
            .map(|file| PathBuf::from(&file.path))
            .collect::<Vec<_>>();

        assert_eq!(files.len(), 2);
        assert_eq!(paths[0], root.path().join("AGENTS.md"));
        assert_eq!(paths[1], project.join("CLAUDE.md"));
    }
}
