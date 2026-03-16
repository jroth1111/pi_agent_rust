use super::improvement::SkillDoctorFormat;
use super::loader::{LoadSkillsOptions, load_skills};
#[cfg(test)]
use super::schema::SkillSections;
use super::schema::{Skill, SkillLineage, infer_skill_name, validate_description, validate_name};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::resources::{DiagnosticKind, ResourceDiagnostic};
use serde::Serialize;
use std::collections::BTreeMap;
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
    pub diagnostic_count: usize,
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
    success_criteria: &[String],
    created_by_skill: Option<&str>,
    created_by_revision: Option<&str>,
    session_id: Option<&str>,
    intended_outcome: Option<&str>,
    baseline: Option<&str>,
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
        success_criteria,
        SkillLineage {
            created_by_skill: normalize_optional_text(created_by_skill)
                .or_else(|| Some("pi-skills-init".to_string())),
            created_by_revision: normalize_optional_text(created_by_revision),
            last_improved_by_skill: None,
            last_improved_by_revision: None,
            session_id: normalize_optional_text(session_id),
            intended_outcome: normalize_optional_text(intended_outcome),
            baseline: normalize_optional_text(baseline),
        },
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
    if !scope_root.exists() {
        let report = SkillLintReport {
            scope: scope_name(global).to_string(),
            skill_count: 0,
            finding_count: 0,
            diagnostic_count: 0,
            skills: Vec::new(),
        };

        match format {
            SkillDoctorFormat::Text => {
                println!("{}", render_skill_lint_report(&report)?);
            }
            SkillDoctorFormat::Json => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|err| Error::session(
                        format!("serialize skill lint report: {err}")
                    ))?
                );
            }
        }

        return Ok(report);
    }

    let loaded = load_skills(LoadSkillsOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: Config::global_dir(),
        skill_paths: vec![scope_root],
        include_defaults: false,
    });

    let diagnostics = loaded.diagnostics;
    let mut skills_by_path = loaded
        .skills
        .into_iter()
        .map(|skill| lint_skill(skill))
        .map(|skill| (skill.skill_path.clone(), skill))
        .collect::<BTreeMap<_, _>>();

    let diagnostic_count = diagnostics.len();
    for diagnostic in diagnostics {
        attach_loader_diagnostic(&mut skills_by_path, diagnostic);
    }

    let mut skills = skills_by_path.into_values().collect::<Vec<_>>();
    skills.sort_by(|left, right| {
        left.skill_name
            .cmp(&right.skill_name)
            .then_with(|| left.skill_path.cmp(&right.skill_path))
    });

    let report = SkillLintReport {
        scope: scope_name(global).to_string(),
        skill_count: skills.len(),
        finding_count: skills.iter().map(|skill| skill.finding_count).sum(),
        diagnostic_count,
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
    success_criteria: &[String],
    lineage: SkillLineage,
    global: bool,
) -> Result<SkillInitReceipt> {
    let name = name.trim();
    let description = description.trim();
    let use_when = use_when.trim();
    let not_for = not_for.trim();
    let trigger_examples = normalize_examples(trigger_examples);
    let anti_trigger_examples = normalize_examples(anti_trigger_examples);
    let success_criteria = normalize_examples(success_criteria);
    let lineage = normalize_lineage(lineage);

    let mut validation_errors = validate_name(name, name);
    validation_errors.extend(validate_description(description));
    if use_when.is_empty() {
        validation_errors.push("`--use-when` is required".to_string());
    }
    if not_for.is_empty() {
        validation_errors.push("`--not-for` is required".to_string());
    }
    if trigger_examples.len() < 2 {
        validation_errors.push(
            "at least two concrete `--trigger` examples are required before scaffolding"
                .to_string(),
        );
    }
    if anti_trigger_examples.len() < 2 {
        validation_errors.push(
            "at least two concrete `--anti-trigger` examples are required before scaffolding"
                .to_string(),
        );
    }
    if success_criteria.is_empty() {
        validation_errors
            .push("at least one `--success-criterion` is required before scaffolding".to_string());
    }
    if lineage.has_creator() && lineage.intended_outcome.is_none() {
        validation_errors
            .push("a creator-attributed skill scaffold requires `--intended-outcome`".to_string());
    }
    if lineage.has_creator() && lineage.baseline.is_none() {
        validation_errors
            .push("a creator-attributed skill scaffold requires `--baseline`".to_string());
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
        &trigger_examples,
        &anti_trigger_examples,
        &success_criteria,
        &lineage,
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
    if skill.lineage.has_creator() {
        if skill.lineage.intended_outcome.is_none() {
            findings.push(SkillLintFinding {
                code: "lineage_missing_intended_outcome".to_string(),
                section: Some("metadata.provenance".to_string()),
                message:
                    "Skills with creator provenance should declare the intended downstream outcome."
                        .to_string(),
            });
        }
        if skill.lineage.baseline.is_none() {
            findings.push(SkillLintFinding {
                code: "lineage_missing_baseline".to_string(),
                section: Some("metadata.provenance".to_string()),
                message: "Skills with creator provenance should record the baseline or workflow they must beat or match."
                    .to_string(),
            });
        }
    }

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
    let substantive_items = items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>();

    if substantive_items.is_empty() {
        findings.push(SkillLintFinding {
            code: format!("missing_{}", code),
            section: Some(section.to_string()),
            message: format!("Add a non-empty `{section}` section to the skill body."),
        });
        return;
    }
    if expect_multiple_examples && substantive_items.len() < 2 {
        findings.push(SkillLintFinding {
            code: format!("{}_too_short", code),
            section: Some(section.to_string()),
            message: format!(
                "`{section}` should include at least two concrete examples to exercise routing boundaries."
            ),
        });
    }
    if substantive_items
        .iter()
        .any(|item| contains_placeholder(item))
    {
        findings.push(SkillLintFinding {
            code: format!("{}_contains_placeholder", code),
            section: Some(section.to_string()),
            message: format!("Replace scaffold placeholders in `{section}` with concrete content."),
        });
    }
}

fn attach_loader_diagnostic(
    skills_by_path: &mut BTreeMap<PathBuf, SkillLintSkillReport>,
    diagnostic: ResourceDiagnostic,
) {
    let path = diagnostic.path;
    let entry = skills_by_path
        .entry(path.clone())
        .or_insert_with(|| SkillLintSkillReport {
            skill_name: skill_name_for_diagnostic(&path),
            skill_path: path.clone(),
            finding_count: 0,
            clean: true,
            findings: Vec::new(),
        });

    entry.findings.push(SkillLintFinding {
        code: match diagnostic.kind {
            DiagnosticKind::Warning => "loader_warning".to_string(),
            DiagnosticKind::Collision => "loader_collision".to_string(),
        },
        section: None,
        message: diagnostic.message,
    });
    entry.finding_count = entry.findings.len();
    entry.clean = false;
}

fn skill_name_for_diagnostic(path: &Path) -> String {
    let inferred = infer_skill_name(path);
    if !inferred.is_empty() {
        return inferred;
    }
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

fn render_skill_template(
    name: &str,
    description: &str,
    use_when: &str,
    not_for: &str,
    trigger_examples: &[String],
    anti_trigger_examples: &[String],
    success_criteria: &[String],
    lineage: &SkillLineage,
) -> String {
    let full_description = build_skill_description(description, use_when, not_for);
    let title = humanize_skill_name(name);
    let frontmatter = render_skill_frontmatter(name, &full_description, lineage);

    format!(
        "{}# {}\n\n## Purpose\n{}\n\n## Use When\n- {}\n\n## Not For\n- {}\n\n## Trigger Examples\n{}\n\n## Anti-Trigger Examples\n{}\n\n## Inputs\n- {} List required inputs, optional inputs, and missing-input behavior.\n\n## Output Contract\n- {} Describe the exact response shape, files, and verification evidence this skill must produce.\n\n## Success Criteria\n- Reach at least 80% successful observed runs across 3 runs.\n- Reach at least 3.5/5 average feedback across 2 ratings.\n{}\n\n## Instructions\n1. Restate the task and confirm the needed inputs.\n2. Follow the output contract exactly.\n3. Verify the result before responding.\n4. State blockers explicitly instead of guessing.\n",
        frontmatter,
        title,
        description.trim_end_matches('.'),
        use_when.trim_end_matches('.'),
        not_for.trim_end_matches('.'),
        format_examples(trigger_examples),
        format_examples(anti_trigger_examples),
        TODO_MARKER,
        TODO_MARKER,
        format_examples(success_criteria),
    )
}

fn render_skill_frontmatter(name: &str, description: &str, lineage: &SkillLineage) -> String {
    let mut out = String::new();
    writeln!(out, "---").expect("write frontmatter");
    writeln!(out, "name: {name}").expect("write frontmatter");
    writeln!(out, "description: {}", yaml_quote(description)).expect("write frontmatter");
    if let Some(metadata) = render_lineage_metadata(lineage) {
        write!(out, "{metadata}").expect("write frontmatter");
    }
    writeln!(out, "---").expect("write frontmatter");
    out
}

fn render_lineage_metadata(lineage: &SkillLineage) -> Option<String> {
    let mut lines = Vec::new();
    push_lineage_entry(
        &mut lines,
        "created-by-skill",
        lineage.created_by_skill.as_deref(),
    );
    push_lineage_entry(
        &mut lines,
        "created-by-revision",
        lineage.created_by_revision.as_deref(),
    );
    push_lineage_entry(
        &mut lines,
        "last-improved-by-skill",
        lineage.last_improved_by_skill.as_deref(),
    );
    push_lineage_entry(
        &mut lines,
        "last-improved-by-revision",
        lineage.last_improved_by_revision.as_deref(),
    );
    push_lineage_entry(&mut lines, "session-id", lineage.session_id.as_deref());
    push_lineage_entry(
        &mut lines,
        "intended-outcome",
        lineage.intended_outcome.as_deref(),
    );
    push_lineage_entry(&mut lines, "baseline", lineage.baseline.as_deref());
    if lines.is_empty() {
        return None;
    }

    let mut out = String::new();
    writeln!(out, "metadata:").expect("write metadata");
    writeln!(out, "  provenance:").expect("write metadata");
    for line in lines {
        writeln!(out, "    {line}").expect("write metadata");
    }
    Some(out)
}

fn push_lineage_entry(lines: &mut Vec<String>, key: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    lines.push(format!("{key}: {}", yaml_quote(value)));
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
    writeln!(out, "Loader diagnostics: {}", report.diagnostic_count)
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

fn normalize_examples(examples: &[String]) -> Vec<String> {
    examples
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
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

fn normalize_optional_text(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn normalize_lineage(mut lineage: SkillLineage) -> SkillLineage {
    lineage.created_by_skill = normalize_optional_text(lineage.created_by_skill.as_deref());
    lineage.created_by_revision = normalize_optional_text(lineage.created_by_revision.as_deref());
    lineage.last_improved_by_skill =
        normalize_optional_text(lineage.last_improved_by_skill.as_deref());
    lineage.last_improved_by_revision =
        normalize_optional_text(lineage.last_improved_by_revision.as_deref());
    lineage.session_id = normalize_optional_text(lineage.session_id.as_deref());
    lineage.intended_outcome = normalize_optional_text(lineage.intended_outcome.as_deref());
    lineage.baseline = normalize_optional_text(lineage.baseline.as_deref());
    lineage
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
            &[
                "run a deploy readiness check".to_string(),
                "audit this release candidate".to_string(),
            ],
            &[
                "fix this production outage".to_string(),
                "write release marketing copy".to_string(),
            ],
            &["must identify all blocking deploy risks before release".to_string()],
            SkillLineage {
                created_by_skill: Some("skill-creator".to_string()),
                created_by_revision: Some("rev-a".to_string()),
                last_improved_by_skill: None,
                last_improved_by_revision: None,
                session_id: Some("sess-7".to_string()),
                intended_outcome: Some(
                    "produce a deploy-readiness skill that catches blockers before release"
                        .to_string(),
                ),
                baseline: Some("manual release checklist review".to_string()),
            },
            false,
        )
        .unwrap();

        let content = fs::read_to_string(&receipt.skill_path).unwrap();
        assert!(content.contains("created-by-skill: \"skill-creator\""));
        assert!(content.contains("intended-outcome: \"produce a deploy-readiness skill that catches blockers before release\""));
        assert!(content.contains("## Trigger Examples"));
        assert!(content.contains("Use when the user wants a release audit or go/no-go decision."));
        assert!(content.contains("- run a deploy readiness check"));
        assert!(content.contains("- audit this release candidate"));
        assert!(content.contains("must identify all blocking deploy risks before release"));
    }

    #[test]
    fn init_requires_concrete_examples_and_success_criteria() {
        let tmp = TempDir::new().unwrap();
        let err = create_skill_scaffold(
            tmp.path(),
            "deploy-readiness",
            "Check deploy readiness before release",
            "the user wants a release audit or go/no-go decision",
            "fixing unrelated product bugs",
            &["run a deploy readiness check".to_string()],
            &["fix this production outage".to_string()],
            &[],
            SkillLineage::default(),
            false,
        )
        .expect_err("expected intake validation failure");

        let message = err.to_string();
        assert!(message.contains("at least two concrete `--trigger` examples"));
        assert!(message.contains("at least two concrete `--anti-trigger` examples"));
        assert!(message.contains("at least one `--success-criterion`"));
    }

    #[test]
    fn init_requires_outcome_and_baseline_for_creator_attribution() {
        let tmp = TempDir::new().unwrap();
        let err = create_skill_scaffold(
            tmp.path(),
            "deploy-readiness",
            "Check deploy readiness before release",
            "the user wants a release audit or go/no-go decision",
            "fixing unrelated product bugs",
            &[
                "run a deploy readiness check".to_string(),
                "audit this release candidate".to_string(),
            ],
            &[
                "fix this production outage".to_string(),
                "write release marketing copy".to_string(),
            ],
            &["must identify all blocking deploy risks before release".to_string()],
            SkillLineage {
                created_by_skill: Some("skill-creator".to_string()),
                created_by_revision: Some("rev-a".to_string()),
                last_improved_by_skill: None,
                last_improved_by_revision: None,
                session_id: None,
                intended_outcome: None,
                baseline: None,
            },
            false,
        )
        .expect_err("expected missing outcome/baseline failure");

        let message = err.to_string();
        assert!(message.contains("`--intended-outcome`"));
        assert!(message.contains("`--baseline`"));
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
    fn lint_includes_loader_diagnostics_for_skipped_skill() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join(".pi/skills/no-description");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: no-description\n---\nBody",
        )
        .unwrap();

        let report = handle_skill_lint(tmp.path(), false, SkillDoctorFormat::Json).unwrap();
        assert_eq!(report.diagnostic_count, 1);
        let skill = report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "no-description")
            .unwrap();
        assert!(
            skill
                .findings
                .iter()
                .any(|finding| finding.code == "loader_warning")
        );
        assert!(
            skill
                .findings
                .iter()
                .any(|finding| finding.message.contains("required"))
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
            &[
                "audit this release plan".to_string(),
                "check whether this release is safe to deploy".to_string(),
            ],
            &[
                "debug a live outage".to_string(),
                "write changelog copy".to_string(),
            ],
            &["must separate blockers from non-blocking release warnings".to_string()],
            SkillLineage::default(),
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
        let skill_dir = tmp.path().join(".pi/skills/sql-review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: sql-review\ndescription: Review SQL queries. Use when the user wants SQL reviewed or improved. Not for general backend design questions.\n---\n## Purpose\nReview SQL queries.\n\n## Use When\n- the user wants SQL reviewed or improved\n\n## Not For\n- general backend design questions\n\n## Trigger Examples\n- TODO: Replace with a concrete request\n- TODO: Add a second distinct example with different wording.\n\n## Anti-Trigger Examples\n- TODO: Replace with a near-miss request that should not activate this skill.\n- TODO: Add a second distinct example with different wording.\n\n## Inputs\n- TODO: List required inputs, optional inputs, and missing-input behavior.\n\n## Output Contract\n- TODO: Describe the exact response shape, files, and verification evidence this skill must produce.\n\n## Success Criteria\n- Reach at least 80% successful observed runs across 3 runs.\n- Reach at least 3.5/5 average feedback across 2 ratings.\n- TODO: Add task-specific thresholds or a baseline this skill must beat.\n\n## Instructions\n1. Restate the task and confirm the needed inputs.\n2. Follow the output contract exactly.\n3. Verify the result before responding.\n4. State blockers explicitly instead of guessing.\n",
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
        assert_eq!(report.diagnostic_count, 0);
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
            lineage: SkillLineage::default(),
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

    #[test]
    fn lint_treats_blank_examples_as_missing() {
        let report = lint_skill(Skill {
            name: "blank-trigger".to_string(),
            description: "Review deploy readiness. Use when the user wants a release audit. Not for production incident response.".to_string(),
            file_path: PathBuf::from("/tmp/blank-trigger/SKILL.md"),
            base_dir: PathBuf::from("/tmp/blank-trigger"),
            source: "test".to_string(),
            disable_model_invocation: false,
            lineage: SkillLineage::default(),
            sections: SkillSections {
                purpose: vec!["Review deploy readiness.".to_string()],
                use_when: vec!["the user wants a release audit".to_string()],
                not_for: vec!["production incident response".to_string()],
                trigger_examples: vec!["".to_string(), "   ".to_string()],
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
                .any(|finding| finding.code == "missing_trigger_examples")
        );
    }
}
