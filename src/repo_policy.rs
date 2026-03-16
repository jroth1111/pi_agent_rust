use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

const MAX_POLICY_FILES: usize = 8;
const MAX_FILE_CHARS: usize = 2_000;
const MAX_TOTAL_CHARS: usize = 6_000;
pub const REPO_POLICY_DIGEST_HEADER: &str = "# Repo Policy Digest";
const REPO_POLICY_START_MARKER: &str = "<pi_repo_policy_digest>";
const REPO_POLICY_END_MARKER: &str = "</pi_repo_policy_digest>";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitRepoPolicyPrompt {
    pub full_prompt: String,
    pub static_prompt: String,
    pub repo_policy: Option<String>,
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

    let mut rendered = format!(
        "{REPO_POLICY_DIGEST_HEADER}\n\nUse these project-specific instructions as binding policy. The raw policy files are loaded from disk but compacted here to bound prompt size.\n\n"
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

pub fn embed_repo_policy_digest(rendered: &str) -> String {
    format!("{REPO_POLICY_START_MARKER}\n{rendered}\n{REPO_POLICY_END_MARKER}")
}

pub fn split_embedded_repo_policy(prompt: &str) -> SplitRepoPolicyPrompt {
    let Some(start_idx) = prompt.find(REPO_POLICY_START_MARKER) else {
        return SplitRepoPolicyPrompt {
            full_prompt: prompt.to_string(),
            static_prompt: prompt.to_string(),
            repo_policy: None,
        };
    };

    let content_start = start_idx + REPO_POLICY_START_MARKER.len();
    let Some(end_rel_idx) = prompt[content_start..].find(REPO_POLICY_END_MARKER) else {
        return SplitRepoPolicyPrompt {
            full_prompt: prompt.to_string(),
            static_prompt: prompt.to_string(),
            repo_policy: None,
        };
    };
    let end_idx = content_start + end_rel_idx;

    let before = &prompt[..start_idx];
    let repo_policy = prompt[content_start..end_idx].trim().to_string();
    let after = &prompt[end_idx + REPO_POLICY_END_MARKER.len()..];
    let full_prompt = format!("{before}{repo_policy}{after}");
    let static_prompt = format!("{before}{after}");

    SplitRepoPolicyPrompt {
        full_prompt,
        static_prompt,
        repo_policy: (!repo_policy.is_empty()).then_some(repo_policy),
    }
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
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn repo_policy_digest_limits_large_files() {
        let digest = build_repo_policy_digest(&[ContextFile {
            path: "/tmp/project/AGENTS.md".to_string(),
            content: "x".repeat(MAX_FILE_CHARS + 500),
        }])
        .expect("digest should exist");

        assert!(digest.truncated);
        assert!(digest.rendered.contains(REPO_POLICY_DIGEST_HEADER));
        assert!(digest.rendered.contains("[truncated]"));
        assert!(digest.rendered.len() < MAX_TOTAL_CHARS + 500);
    }

    #[test]
    fn split_embedded_repo_policy_extracts_marked_section() {
        let prompt = format!(
            "Base instructions.\n\n{}\n\nCurrent date and time: <TIMESTAMP>",
            embed_repo_policy_digest("# Repo Policy Digest\n\nAlways test.")
        );

        let split = split_embedded_repo_policy(&prompt);

        assert!(split.full_prompt.contains(REPO_POLICY_DIGEST_HEADER));
        assert!(!split.full_prompt.contains(REPO_POLICY_START_MARKER));
        assert!(!split.full_prompt.contains(REPO_POLICY_END_MARKER));
        assert_eq!(
            split.repo_policy.as_deref(),
            Some("# Repo Policy Digest\n\nAlways test.")
        );
        assert!(!split.static_prompt.contains(REPO_POLICY_DIGEST_HEADER));
        assert!(split.static_prompt.contains("Current date and time"));
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
