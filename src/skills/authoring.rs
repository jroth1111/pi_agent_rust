use super::improvement::SkillDoctorFormat;
use super::loader::{LoadSkillsOptions, load_skills};
#[cfg(test)]
use super::schema::SkillSections;
use super::schema::{Skill, validate_description, validate_name};
use crate::config::Config;
use crate::error::{Error, Result};
use serde::Serialize;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const PROJECT_SCOPE: &str = "project";
const GLOBAL_SCOPE: &str = "global";
const TODO_MARKER: &str = "TODO:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInitReceipt {
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub scope: String,
    pub placeholder_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillLintFinding {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillLintSkillReport {
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub finding_count: usize,
    pub clean: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<SkillLintFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillLintReport {
    pub scope: String,
    pub skill_count: usize,
    pub finding_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillLintSkillReport>,
}

pub fn handle_skill_init(
    cwd: &Path,
    name: &str,
    description: &str,
    use_when: &str,
    not_for: &str,
    trigger_examples: &[String],
    anti_trigger_examples: &[String],
    global: bool,
    format: SkillDoctorFormat,
) -> Result<SkillInitReceipt> {
    let receipt = create_skill_scaffold(
        cwd,
        name,
        description,
        use_when,
        not_for,
        trigger_examples,
        anti_trigger_examples,
        global,
    )?;

    match format {
        SkillDoctorFormat::Text => {
            println!("{}", render_skill_init_receipt(&receipt)?);
        }
        SkillDoctorFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&receipt).map_err(|err| Error::session(format!(
                    "serialize skill init receipt: {err}"
                )))?
            );
        }
    }

    Ok(receipt)
}

pub fn handle_skill_lint(
    cwd: &Path,
    global: bool,
    format: SkillDoctorFormat,
) -> Result<SkillLintReport> {
    let scope_root = skill_scope_root(cwd, global);
    let loaded = load_skills(LoadSkillsOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: Config::global_dir(),
        skill_paths: vec![scope_root],
        include_defaults: false,
    });

    let mut skills = loaded
        .skills
        .into_iter()
        .map(|skill| lint_skill(skill))
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| left.skill_name.cmp(&right.skill_name));

    let report = SkillLintReport {
        scope: scope_name(global).to_string(),
        skill_count: skills.len(),
        finding_count: skills.iter().map(|skill| skill.finding_count).sum(),
        skills,
    };

    match format {
        SkillDoctorFormat::Text => {
            println!("{}", render_skill_lint_report(&report)?);
        }
        SkillDoctorFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .map_err(|err| Error::session(format!("serialize skill lint report: {err}")))?
            );
        }
    }

    Ok(report)
}

fn create_skill_scaffold(
    cwd: &Path,
    name: &str,
    description: &str,
    use_when: &str,
    not_for: &str,
    trigger_examples: &[String],
    anti_trigger_examples: &[String],
    global: bool,
) -> Result<SkillInitReceipt> {
    let name = name.trim();
    let description = description.trim();
    let use_when = use_when.trim();
    let not_for = not_for.trim();

    let mut validation_errors = validate_name(name, name);
    validation_errors.extend(validate_description(description));
    if use_when.is_empty() {
        validation_errors.push("`--use-when` is required".to_string());
    }
    if not_for.is_empty() {
        validation_errors.push("`--not-for` is required".to_string());
    }
    if !validation_errors.is_empty() {
        return Err(Error::validation(validation_errors.join("; ")));
    }

    let skill_dir = skill_scope_root(cwd, global).join(name);
    let skill_path = skill_dir.join("SKILL.md");
    if skill_path.exists() {
        return Err(Error::validation(format!(
            "Skill scaffold already exists at {}",
            skill_path.display()
        )));
    }

    fs::create_dir_all(&skill_dir).map_err(|err| {
        Error::config(format!(
            "failed to create skill directory {}: {err}",
            skill_dir.display()
        ))
    })?;

    let body = render_skill_template(
        name,
        description,
        use_when,
        not_for,
        trigger_examples,
        anti_trigger_examples,
    );
    fs::write(&skill_path, body.as_bytes()).map_err(|err| {
        Error::config(format!(
            "failed to write skill scaffold {}: {err}",
            skill_path.display()
        ))
    })?;

    Ok(SkillInitReceipt {
        skill_name: name.to_string(),
        skill_path,
        scope: scope_name(global).to_string(),
        placeholder_count: body.match_indices(TODO_MARKER).count(),
    })
}

fn lint_skill(skill: Skill) -> SkillLintSkillReport {
    let mut findings = Vec::new();
    if !skill.description.to_ascii_lowercase().contains("use when") {
        findings.push(SkillLintFinding {
            code: "description_missing_use_when".to_string(),
            section: Some("description".to_string()),
            message: "Description should include a concrete `Use when ...` routing boundary."
                .to_string(),
        });
    }
    if !skill.description.to_ascii_lowercase().contains("not for") {
        findings.push(SkillLintFinding {
            code: "description_missing_not_for".to_string(),
            section: Some("description".to_string()),
            message: "Description should include a concrete `Not for ...` boundary.".to_string(),
        });
    }

    lint_required_section(
        &mut findings,
        "purpose",
        "Purpose",
        &skill.sections.purpose,
        false,
    );
    lint_required_section(
        &mut findings,
        "use_when",
        "Use When",
        &skill.sections.use_when,
        false,
    );
    lint_required_section(
        &mut findings,
        "not_for",
        "Not For",
        &skill.sections.not_for,
        false,
    );
    lint_required_section(
        &mut findings,
        "trigger_examples",
        "Trigger Examples",
        &skill.sections.trigger_examples,
        true,
    );
    lint_required_section(
        &mut findings,
        "anti_trigger_examples",
        "Anti-Trigger Examples",
        &skill.sections.anti_trigger_examples,
        true,
    );
    lint_required_section(
        &mut findings,
        "inputs",
        "Inputs",
        &skill.sections.inputs,
        false,
    );
    lint_required_section(
        &mut findings,
        "output_contract",
        "Output Contract",
        &skill.sections.output_contract,
        false,
    );
    lint_required_section(
        &mut findings,
        "success_criteria",
        "Success Criteria",
        &skill.sections.success_criteria,
        false,
    );
    lint_required_section(
        &mut findings,
        "instructions",
        "Instructions",
        &skill.sections.instructions,
        false,
    );

    SkillLintSkillReport {
        skill_name: skill.name,
        skill_path: skill.file_path,
        finding_count: findings.len(),
        clean: findings.is_empty(),
        findings,
    }
}

fn lint_required_section(
    findings: &mut Vec<SkillLintFinding>,
    code: &str,
    section: &str,
    items: &[String],
    expect_multiple_examples: bool,
) {
    if items.is_empty() {
        findings.push(SkillLintFinding {
            code: format!("missing_{}", code),
            section: Some(section.to_string()),
            message: format!("Add a non-empty `{section}` section to the skill body."),
        });
        return;
    }
    if expect_multiple_examples && items.len() < 2 {
        findings.push(SkillLintFinding {
            code: format!("{}_too_short", code),
            section: Some(section.to_string()),
            message: format!(
                "`{section}` should include at least two concrete examples to exercise routing boundaries."
            ),
        });
    }
    if items.iter().any(|item| contains_placeholder(item)) {
        findings.push(SkillLintFinding {
            code: format!("{}_contains_placeholder", code),
            section: Some(section.to_string()),
            message: format!("Replace scaffold placeholders in `{section}` with concrete content."),
        });
    }
}

fn render_skill_template(
    name: &str,
    description: &str,
    use_when: &str,
    not_for: &str,
    trigger_examples: &[String],
    anti_trigger_examples: &[String],
) -> String {
    let full_description = build_skill_description(description, use_when, not_for);
    let title = humanize_skill_name(name);

    let trigger_examples = scaffold_examples(
        trigger_examples,
        "Replace with a concrete request that should activate this skill.",
    );
    let anti_trigger_examples = scaffold_examples(
        anti_trigger_examples,
        "Replace with a near-miss request that should not activate this skill.",
    );

    format!(
        "---\nname: {}\ndescription: {}\n---\n# {}\n\n## Purpose\n{}\n\n## Use When\n- {}\n\n## Not For\n- {}\n\n## Trigger Examples\n{}\n\n## Anti-Trigger Examples\n{}\n\n## Inputs\n- {} List required inputs, optional inputs, and missing-input behavior.\n\n## Output Contract\n- {} Describe the exact response shape, files, and verification evidence this skill must produce.\n\n## Success Criteria\n- Reach at least 80% successful observed runs across 3 runs.\n- Reach at least 3.5/5 average feedback across 2 ratings.\n- {} Add task-specific thresholds or a baseline this skill must beat.\n\n## Instructions\n1. Restate the task and confirm the needed inputs.\n2. Follow the output contract exactly.\n3. Verify the result before responding.\n4. State blockers explicitly instead of guessing.\n",
        name,
        yaml_quote(&full_description),
        title,
        description.trim_end_matches('.'),
        use_when.trim_end_matches('.'),
        not_for.trim_end_matches('.'),
        format_examples(&trigger_examples),
        format_examples(&anti_trigger_examples),
        TODO_MARKER,
        TODO_MARKER,
        TODO_MARKER,
    )
}

fn render_skill_init_receipt(receipt: &SkillInitReceipt) -> Result<String> {
    let mut out = String::new();
    writeln!(
        out,
        "Created {} skill scaffold `{}`",
        receipt.scope, receipt.skill_name
    )
    .map_err(|err| Error::session(format!("render skill init receipt: {err}")))?;
    writeln!(out, "  path: {}", receipt.skill_path.display())
        .map_err(|err| Error::session(format!("render skill init receipt: {err}")))?;
    writeln!(
        out,
        "  placeholders: {} (run `pi skills lint` after replacing them)",
        receipt.placeholder_count
    )
    .map_err(|err| Error::session(format!("render skill init receipt: {err}")))?;
    Ok(out)
}

fn render_skill_lint_report(report: &SkillLintReport) -> Result<String> {
    let mut out = String::new();
    writeln!(out, "Skill lint scope: {}", report.scope)
        .map_err(|err| Error::session(format!("render skill lint report: {err}")))?;
    writeln!(out, "Skills linted: {}", report.skill_count)
        .map_err(|err| Error::session(format!("render skill lint report: {err}")))?;
    writeln!(out, "Findings: {}", report.finding_count)
        .map_err(|err| Error::session(format!("render skill lint report: {err}")))?;
    if report.skills.is_empty() {
        writeln!(out, "No skills found in scope.")
            .map_err(|err| Error::session(format!("render skill lint report: {err}")))?;
        return Ok(out);
    }
    for skill in &report.skills {
        writeln!(
            out,
            "{}: {}",
            skill.skill_name,
            if skill.clean {
                "clean".to_string()
            } else {
                format!("{} finding(s)", skill.finding_count)
            }
        )
        .map_err(|err| Error::session(format!("render skill lint report: {err}")))?;
        writeln!(out, "  path: {}", skill.skill_path.display())
            .map_err(|err| Error::session(format!("render skill lint report: {err}")))?;
        for finding in &skill.findings {
            writeln!(out, "  - {}: {}", finding.code, finding.message)
                .map_err(|err| Error::session(format!("render skill lint report: {err}")))?;
        }
    }
    Ok(out)
}

fn skill_scope_root(cwd: &Path, global: bool) -> PathBuf {
    if global {
        Config::global_dir().join("skills")
    } else {
        cwd.join(Config::project_dir()).join("skills")
    }
}

fn scope_name(global: bool) -> &'static str {
    if global { GLOBAL_SCOPE } else { PROJECT_SCOPE }
}

fn build_skill_description(description: &str, use_when: &str, not_for: &str) -> String {
    format!(
        "{}. Use when {}. Not for {}.",
        trim_sentence(description),
        trim_sentence(use_when),
        trim_sentence(not_for),
    )
}

fn trim_sentence(value: &str) -> &str {
    value.trim().trim_end_matches('.')
}

fn scaffold_examples(examples: &[String], placeholder: &str) -> Vec<String> {
    if examples.is_empty() {
        vec![
            format!("{TODO_MARKER} {placeholder}"),
            format!("{TODO_MARKER} Add a second distinct example with different wording."),
        ]
    } else {
        examples
            .iter()
            .map(|value| value.trim().to_string())
            .collect()
    }
}

fn format_examples(examples: &[String]) -> String {
    examples
        .iter()
        .map(|example| format!("- {example}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn humanize_skill_name(name: &str) -> String {
    name.split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    let mut out = first.to_ascii_uppercase().to_string();
                    out.push_str(chars.as_str());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn contains_placeholder(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    normalized.contains("todo:")
        || normalized.contains("fill in")
        || normalized.contains("replace with a concrete request")
        || normalized.contains("replace with a near-miss request")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn init_creates_project_skill_scaffold() {
        let tmp = TempDir::new().unwrap();
        let receipt = create_skill_scaffold(
            tmp.path(),
            "deploy-readiness",
            "Check deploy readiness before release",
            "the user wants a release audit or go/no-go decision",
            "fixing unrelated product bugs",
            &[],
            &[],
            false,
        )
        .unwrap();

        let content = fs::read_to_string(&receipt.skill_path).unwrap();
        assert!(content.contains("## Trigger Examples"));
        assert!(content.contains("Use when the user wants a release audit or go/no-go decision."));
        assert!(content.contains("TODO: Replace with a concrete request"));
    }

    #[test]
    fn lint_reports_missing_authoring_structure() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join(".pi/skills/simple-review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: simple-review\ndescription: Review code carefully\n---\nBody only",
        )
        .unwrap();

        let report = handle_skill_lint(tmp.path(), false, SkillDoctorFormat::Json).unwrap();
        let skill = report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "simple-review")
            .unwrap();
        assert!(!skill.clean);
        assert!(
            skill
                .findings
                .iter()
                .any(|finding| finding.code == "missing_use_when")
        );
        assert!(
            skill
                .findings
                .iter()
                .any(|finding| finding.code == "description_missing_not_for")
        );
    }

    #[test]
    fn lint_accepts_complete_skill_structure() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join(".pi/skills/deploy-readiness");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy-readiness\ndescription: Check deploy readiness. Use when the user wants a release audit or go/no-go decision. Not for fixing unrelated product bugs.\n---\n## Purpose\nCheck deploy readiness.\n\n## Use When\n- the user wants a release audit\n- the user wants a go/no-go decision\n\n## Not For\n- fixing unrelated product bugs\n\n## Trigger Examples\n- run a deploy readiness check\n- audit this release candidate\n\n## Anti-Trigger Examples\n- fix this production outage\n- write release marketing copy\n\n## Inputs\n- release branch\n\n## Output Contract\n- concise checklist with blockers\n\n## Success Criteria\n- no critical blockers missed\n\n## Instructions\n1. inspect the release diff",
        )
        .unwrap();

        let report = handle_skill_lint(tmp.path(), false, SkillDoctorFormat::Json).unwrap();
        let skill = report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "deploy-readiness")
            .unwrap();
        assert!(skill.clean);
        assert!(skill.findings.is_empty());
    }

    #[test]
    fn scaffolded_skill_is_discoverable() {
        let tmp = TempDir::new().unwrap();
        create_skill_scaffold(
            tmp.path(),
            "release-audit",
            "Audit a release plan",
            "the user wants release readiness checked",
            "incident response",
            &["audit this release plan".to_string()],
            &[
                "debug a live outage".to_string(),
                "write changelog copy".to_string(),
            ],
            false,
        )
        .unwrap();

        let loaded = load_skills(LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: Config::global_dir(),
            skill_paths: vec![tmp.path().join(".pi/skills")],
            include_defaults: false,
        });
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.skills[0].name, "release-audit");
    }

    #[test]
    fn lint_flags_placeholder_sections() {
        let tmp = TempDir::new().unwrap();
        create_skill_scaffold(
            tmp.path(),
            "sql-review",
            "Review SQL queries",
            "the user wants SQL reviewed or improved",
            "general backend design questions",
            &[],
            &[],
            false,
        )
        .unwrap();

        let report = handle_skill_lint(tmp.path(), false, SkillDoctorFormat::Json).unwrap();
        let skill = report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "sql-review")
            .unwrap();
        assert!(
            skill
                .findings
                .iter()
                .any(|finding| finding.code == "trigger_examples_contains_placeholder")
        );
    }

    #[test]
    fn build_skill_description_uses_routing_formula() {
        assert_eq!(
            build_skill_description(
                "Check deploy readiness.",
                "the user wants a release audit.",
                "fixing unrelated bugs."
            ),
            "Check deploy readiness. Use when the user wants a release audit. Not for fixing unrelated bugs."
        );
    }

    #[test]
    fn lint_uses_project_scope_by_default() {
        let tmp = TempDir::new().unwrap();
        let report = handle_skill_lint(tmp.path(), false, SkillDoctorFormat::Json).unwrap();
        assert_eq!(report.scope, PROJECT_SCOPE);
        assert_eq!(report.skill_count, 0);
    }

    #[test]
    fn humanize_skill_name_title_cases_slug() {
        assert_eq!(
            humanize_skill_name("deploy-readiness-checker"),
            "Deploy Readiness Checker"
        );
    }

    #[test]
    fn contains_placeholder_detects_scaffold_markers() {
        assert!(contains_placeholder("TODO: replace me"));
        assert!(contains_placeholder(
            "Replace with a concrete request that should activate this skill."
        ));
        assert!(!contains_placeholder(
            "Use the read tool to inspect deployment configs."
        ));
    }

    #[test]
    fn lint_finds_single_trigger_example_as_too_short() {
        let report = lint_skill(Skill {
            name: "single-trigger".to_string(),
            description: "Review deploy readiness. Use when the user wants a release audit. Not for production incident response.".to_string(),
            file_path: PathBuf::from("/tmp/single-trigger/SKILL.md"),
            base_dir: PathBuf::from("/tmp/single-trigger"),
            source: "test".to_string(),
            disable_model_invocation: false,
            sections: SkillSections {
                purpose: vec!["Review deploy readiness.".to_string()],
                use_when: vec!["the user wants a release audit".to_string()],
                not_for: vec!["production incident response".to_string()],
                trigger_examples: vec!["run a deploy readiness check".to_string()],
                anti_trigger_examples: vec![
                    "fix a live outage".to_string(),
                    "draft release notes".to_string(),
                ],
                inputs: vec!["release branch".to_string()],
                output_contract: vec!["concise checklist".to_string()],
                success_criteria: vec!["no critical blockers missed".to_string()],
                instructions: vec!["inspect the release diff".to_string()],
            },
        });
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "trigger_examples_too_short")
        );
    }
}
