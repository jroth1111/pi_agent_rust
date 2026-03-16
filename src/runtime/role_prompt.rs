use crate::config::Config;
use crate::repo_policy::{
    build_repo_policy_digest, embed_repo_policy_digest, load_project_context_files,
};
use chrono::{Datelike, Local};
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Interactive,
    Orchestrator,
    Worker,
    Validator,
}

pub fn build_role_system_prompt(role: SessionRole, cwd: &Path, enabled_tools: &[&str]) -> String {
    let mut prompt = match role {
        SessionRole::Interactive => String::from(
            "You are an interactive coding assistant. Balance exploration and execution, keep changes scoped, and prefer direct evidence over assumptions.",
        ),
        SessionRole::Orchestrator => String::from(
            "You are the orchestrator for a durable runtime mission. Plan, delegate, and validate progress across worker sessions. Do not implement worker task details yourself when a worker contract exists.",
        ),
        SessionRole::Worker => String::from(
            "You are a worker session for a durable runtime mission. Execute exactly the assigned task, stay within scope, prefer direct tool evidence over guesses, run verification before finishing when possible, and report blockers explicitly instead of silently stopping.",
        ),
        SessionRole::Validator => String::from(
            "You are a validator session for a durable runtime mission. Verify the declared assertions with direct evidence, fail loudly on gaps, and do not rewrite the task scope to make it easier to pass.",
        ),
    };

    if !enabled_tools.is_empty() {
        let _ = write!(prompt, "\n\nEnabled tools: {}.", enabled_tools.join(", "));
    }

    if matches!(role, SessionRole::Worker) {
        prompt.push_str(
            "\n\nWorker constraints:\n- Treat the user message as the task manifest.\n- Do not re-plan the overall mission.\n- Keep the diff minimal and within planned touches.\n- If verification fails, say so clearly and include the blocker.\n- End with a concise implementation summary grounded in files changed and verification status.",
        );
    }

    let global_dir = Config::global_dir();
    let context_files = load_project_context_files(cwd, &global_dir);
    if let Some(repo_policy) = build_repo_policy_digest(&context_files) {
        prompt.push_str("\n\n");
        prompt.push_str(&embed_repo_policy_digest(&repo_policy.rendered));
    }

    let _ = write!(
        prompt,
        "\nCurrent date and time: {}",
        format_current_datetime()
    );
    let _ = write!(prompt, "\nCurrent working directory: {}", cwd.display());
    prompt
}

fn format_current_datetime() -> String {
    let now = Local::now();
    let date = format!(
        "{}, {} {}, {}",
        now.format("%A"),
        now.format("%B"),
        now.day(),
        now.year()
    );
    let time = format!("{} {}", now.format("%I:%M:%S %p"), now.format("%Z"));
    format!("{date}, {time}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn worker_prompt_includes_repo_policy_digest() {
        let root = tempdir().expect("tempdir");
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        std::fs::write(project.join("AGENTS.md"), "be precise").expect("write policy");

        let prompt = build_role_system_prompt(SessionRole::Worker, &project, &["read", "edit"]);
        assert!(prompt.contains("worker session for a durable runtime mission"));
        assert!(prompt.contains("# Repo Policy Digest"));
        assert!(prompt.contains("be precise"));
        assert!(prompt.contains("Enabled tools: read, edit."));
    }
}
