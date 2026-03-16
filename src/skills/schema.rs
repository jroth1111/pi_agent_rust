use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;

pub const MAX_SKILL_NAME_LEN: usize = 64;
pub const MAX_SKILL_DESC_LEN: usize = 1024;

pub const ALLOWED_SKILL_FRONTMATTER: [&str; 7] = [
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "allowed-tools",
    "disable-model-invocation",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillSections {
    pub purpose: Vec<String>,
    pub use_when: Vec<String>,
    pub not_for: Vec<String>,
    pub trigger_examples: Vec<String>,
    pub anti_trigger_examples: Vec<String>,
    pub inputs: Vec<String>,
    pub output_contract: Vec<String>,
    pub success_criteria: Vec<String>,
    pub instructions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillLineage {
    pub created_by_skill: Option<String>,
    pub created_by_revision: Option<String>,
    pub last_improved_by_skill: Option<String>,
    pub last_improved_by_revision: Option<String>,
    pub session_id: Option<String>,
    pub intended_outcome: Option<String>,
    pub baseline: Option<String>,
}

impl SkillLineage {
    pub fn is_empty(&self) -> bool {
        self.created_by_skill.is_none()
            && self.created_by_revision.is_none()
            && self.last_improved_by_skill.is_none()
            && self.last_improved_by_revision.is_none()
            && self.session_id.is_none()
            && self.intended_outcome.is_none()
            && self.baseline.is_none()
    }

    pub fn has_creator(&self) -> bool {
        self.created_by_skill.is_some()
    }

    pub fn has_improver(&self) -> bool {
        self.last_improved_by_skill.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source: String,
    pub disable_model_invocation: bool,
    pub sections: SkillSections,
    pub lineage: SkillLineage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitSkillInvocation {
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputExpansion {
    pub text: String,
    pub explicit_skill: Option<ExplicitSkillInvocation>,
}

pub fn validate_name(name: &str, parent_dir: &str) -> Vec<String> {
    let mut errors = Vec::new();

    if name != parent_dir {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir}\""
        ));
    }

    if name.len() > MAX_SKILL_NAME_LEN {
        errors.push(format!(
            "name exceeds {MAX_SKILL_NAME_LEN} characters ({})",
            name.len()
        ));
    }

    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }

    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }

    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }

    errors
}

pub fn validate_description(description: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if description.trim().is_empty() {
        errors.push("description is required".to_string());
    } else if description.len() > MAX_SKILL_DESC_LEN {
        errors.push(format!(
            "description exceeds {MAX_SKILL_DESC_LEN} characters ({})",
            description.len()
        ));
    }
    errors
}

pub fn validate_frontmatter_fields<'a, I>(keys: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a String>,
{
    let allowed: HashSet<&str> = ALLOWED_SKILL_FRONTMATTER.into_iter().collect();
    let mut errors = Vec::new();
    for key in keys {
        if !allowed.contains(key.as_str()) {
            errors.push(format!("unknown frontmatter field \"{key}\""));
        }
    }
    errors
}

pub fn parse_skill_sections(body: &str) -> SkillSections {
    let parsed = parse_markdown_sections(body);
    SkillSections {
        purpose: parsed.get("purpose").cloned().unwrap_or_default(),
        use_when: parsed.get("use when").cloned().unwrap_or_default(),
        not_for: parsed.get("not for").cloned().unwrap_or_default(),
        trigger_examples: parsed.get("trigger examples").cloned().unwrap_or_default(),
        anti_trigger_examples: parsed
            .get("anti trigger examples")
            .or_else(|| parsed.get("anti trigger example"))
            .cloned()
            .unwrap_or_default(),
        inputs: parsed.get("inputs").cloned().unwrap_or_default(),
        output_contract: parsed.get("output contract").cloned().unwrap_or_default(),
        success_criteria: parsed.get("success criteria").cloned().unwrap_or_default(),
        instructions: parsed.get("instructions").cloned().unwrap_or_default(),
    }
}

pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description."
            .to_string(),
        "Prefer skills whose `use_when` and trigger examples match the user request, and avoid skills whose `not_for` or anti-trigger examples match."
            .to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];

    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.display().to_string())
        ));
        if !skill.sections.use_when.is_empty() {
            lines.push(format!(
                "    <use_when>{}</use_when>",
                escape_xml(&summarize_section_items(&skill.sections.use_when, 2))
            ));
        }
        if !skill.sections.not_for.is_empty() {
            lines.push(format!(
                "    <not_for>{}</not_for>",
                escape_xml(&summarize_section_items(&skill.sections.not_for, 2))
            ));
        }
        if !skill.sections.trigger_examples.is_empty() {
            lines.push(format!(
                "    <trigger_examples>{}</trigger_examples>",
                escape_xml(&summarize_section_items(
                    &skill.sections.trigger_examples,
                    2
                ))
            ));
        }
        if !skill.sections.anti_trigger_examples.is_empty() {
            lines.push(format!(
                "    <anti_trigger_examples>{}</anti_trigger_examples>",
                escape_xml(&summarize_section_items(
                    &skill.sections.anti_trigger_examples,
                    2
                ))
            ));
        }
        lines.push("  </skill>".to_string());
    }

    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

pub fn expand_skill_command(text: &str, skills: &[Skill]) -> String {
    expand_skill_command_with_trace(text, skills).text
}

pub fn expand_skill_command_with_trace(text: &str, skills: &[Skill]) -> InputExpansion {
    if !text.starts_with("/skill:") {
        return InputExpansion {
            text: text.to_string(),
            explicit_skill: None,
        };
    }

    let space_index = text.find(' ');
    let name = space_index.map_or(&text[7..], |idx| &text[7..idx]);
    let args = space_index.map_or("", |idx| text[idx + 1..].trim());

    let Some(skill) = skills.iter().find(|s| s.name == name) else {
        return InputExpansion {
            text: text.to_string(),
            explicit_skill: None,
        };
    };

    match fs::read_to_string(&skill.file_path) {
        Ok(content) => {
            let body = strip_frontmatter(&content).trim().to_string();
            let block = format!(
                "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                skill.name,
                skill.file_path.display(),
                skill.base_dir.display(),
                body
            );
            let text = if args.is_empty() {
                block
            } else {
                format!("{block}\n\n{args}")
            };
            InputExpansion {
                text,
                explicit_skill: Some(ExplicitSkillInvocation {
                    skill_name: skill.name.clone(),
                    skill_path: skill.file_path.clone(),
                    args: args.to_string(),
                }),
            }
        }
        Err(err) => {
            eprintln!(
                "Warning: Failed to read skill {}: {err}",
                skill.file_path.display()
            );
            InputExpansion {
                text: text.to_string(),
                explicit_skill: None,
            }
        }
    }
}

pub(crate) fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[derive(Debug, Clone)]
pub struct ParsedFrontmatter {
    pub frontmatter: HashMap<String, YamlValue>,
    pub body: String,
    pub error: Option<String>,
}

pub fn frontmatter_string(frontmatter: &HashMap<String, YamlValue>, key: &str) -> Option<String> {
    frontmatter.get(key).and_then(frontmatter_scalar_string)
}

pub fn frontmatter_bool(frontmatter: &HashMap<String, YamlValue>, key: &str) -> Option<bool> {
    match frontmatter.get(key)? {
        YamlValue::Bool(value) => Some(*value),
        YamlValue::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub fn parse_skill_lineage(frontmatter: &HashMap<String, YamlValue>) -> SkillLineage {
    let Some(metadata) = frontmatter_mapping(frontmatter, "metadata") else {
        return SkillLineage::default();
    };
    let source = mapping_value(metadata, "provenance")
        .and_then(YamlValue::as_mapping)
        .unwrap_or(metadata);

    SkillLineage {
        created_by_skill: mapping_string(source, "created-by-skill"),
        created_by_revision: mapping_string(source, "created-by-revision"),
        last_improved_by_skill: mapping_string(source, "last-improved-by-skill"),
        last_improved_by_revision: mapping_string(source, "last-improved-by-revision"),
        session_id: mapping_string(source, "session-id"),
        intended_outcome: mapping_string(source, "intended-outcome"),
        baseline: mapping_string(source, "baseline"),
    }
}

fn frontmatter_scalar_string(value: &YamlValue) -> Option<String> {
    match value {
        YamlValue::Null => None,
        YamlValue::Bool(value) => Some(value.to_string()),
        YamlValue::Number(value) => Some(value.to_string()),
        YamlValue::String(value) => Some(value.clone()),
        _ => None,
    }
}

fn frontmatter_mapping<'a>(
    frontmatter: &'a HashMap<String, YamlValue>,
    key: &str,
) -> Option<&'a serde_yaml::Mapping> {
    frontmatter.get(key)?.as_mapping()
}

fn mapping_value<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a YamlValue> {
    mapping.get(YamlValue::String(key.to_string()))
}

fn mapping_string(mapping: &serde_yaml::Mapping, key: &str) -> Option<String> {
    mapping_value(mapping, key).and_then(frontmatter_scalar_string)
}

pub fn parse_frontmatter(raw: &str) -> ParsedFrontmatter {
    let mut lines = raw.lines();
    let Some(first) = lines.next() else {
        return ParsedFrontmatter {
            frontmatter: HashMap::new(),
            body: String::new(),
            error: None,
        };
    };

    if first.trim() != "---" {
        return ParsedFrontmatter {
            frontmatter: HashMap::new(),
            body: raw.to_string(),
            error: None,
        };
    }

    let mut front_lines = Vec::new();
    let mut body_lines = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
                continue;
            }
            front_lines.push(line);
        } else {
            body_lines.push(line);
        }
    }

    if in_frontmatter {
        return ParsedFrontmatter {
            frontmatter: HashMap::new(),
            body: raw.to_string(),
            error: Some("unclosed YAML frontmatter".to_string()),
        };
    }

    let (frontmatter, error) = parse_frontmatter_yaml(&front_lines.join("\n"));

    ParsedFrontmatter {
        frontmatter,
        body: body_lines.join("\n"),
        error,
    }
}

fn parse_frontmatter_yaml(raw: &str) -> (HashMap<String, YamlValue>, Option<String>) {
    if raw.trim().is_empty() {
        return (HashMap::new(), None);
    }

    let parsed = match serde_yaml::from_str::<YamlValue>(raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            return (
                HashMap::new(),
                Some(format!("invalid YAML frontmatter: {err}")),
            );
        }
    };

    let YamlValue::Mapping(mapping) = parsed else {
        return (
            HashMap::new(),
            Some("YAML frontmatter root must be a mapping".to_string()),
        );
    };

    let mut map = HashMap::new();
    let mut invalid_keys = Vec::new();
    for (key, value) in mapping {
        match key {
            YamlValue::String(key) if !key.is_empty() => {
                map.insert(key, value);
            }
            YamlValue::String(_) => {}
            other => invalid_keys.push(render_yaml_value(&other)),
        }
    }

    let error = (!invalid_keys.is_empty()).then(|| {
        format!(
            "YAML frontmatter keys must be strings; unsupported key(s): {}",
            invalid_keys.join(", ")
        )
    });
    (map, error)
}

fn render_yaml_value(value: &YamlValue) -> String {
    serde_yaml::to_string(value)
        .unwrap_or_else(|_| format!("{value:?}"))
        .trim()
        .to_string()
}

fn parse_markdown_sections(body: &str) -> HashMap<String, Vec<String>> {
    let mut sections = HashMap::new();
    let mut current_heading: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = markdown_heading_name(trimmed) {
            flush_markdown_section(&mut sections, current_heading.take(), &current_lines);
            current_heading = Some(heading);
            current_lines.clear();
            continue;
        }

        if current_heading.is_some() {
            current_lines.push(line.to_string());
        }
    }

    flush_markdown_section(&mut sections, current_heading.take(), &current_lines);
    sections
}

fn flush_markdown_section(
    sections: &mut HashMap<String, Vec<String>>,
    heading: Option<String>,
    lines: &[String],
) {
    let Some(heading) = heading else {
        return;
    };
    let items = parse_section_items(lines);
    if items.is_empty() {
        return;
    }
    sections.insert(heading, items);
}

fn markdown_heading_name(trimmed: &str) -> Option<String> {
    let hash_count = trimmed.chars().take_while(|ch| *ch == '#').count();
    if hash_count < 2 {
        return None;
    }
    let title = trimmed[hash_count..].trim();
    if title.is_empty() {
        return None;
    }
    Some(normalize_heading(title))
}

fn normalize_heading(title: &str) -> String {
    title
        .trim()
        .trim_end_matches(':')
        .chars()
        .map(|ch| match ch {
            '-' | '_' => ' ',
            _ => ch.to_ascii_lowercase(),
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_section_items(lines: &[String]) -> Vec<String> {
    let mut items = Vec::new();
    let mut paragraph = Vec::new();

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_paragraph(&mut items, &mut paragraph);
            continue;
        }
        if trimmed.starts_with("<!--") {
            continue;
        }
        if let Some(item) = strip_list_marker(trimmed) {
            flush_paragraph(&mut items, &mut paragraph);
            if !item.is_empty() {
                items.push(item.to_string());
            }
            continue;
        }
        paragraph.push(trimmed.to_string());
    }

    flush_paragraph(&mut items, &mut paragraph);
    items
}

fn flush_paragraph(items: &mut Vec<String>, paragraph: &mut Vec<String>) {
    if paragraph.is_empty() {
        return;
    }
    items.push(paragraph.join(" "));
    paragraph.clear();
}

fn strip_list_marker(trimmed: &str) -> Option<&str> {
    if trimmed == "-" || trimmed == "*" {
        return Some("");
    }
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| strip_numbered_list_marker(trimmed))
        .map(str::trim)
}

fn strip_numbered_list_marker(trimmed: &str) -> Option<&str> {
    let period_index = trimmed.find(". ")?;
    trimmed[..period_index]
        .chars()
        .all(|ch| ch.is_ascii_digit())
        .then_some(&trimmed[period_index + 2..])
}

fn summarize_section_items(items: &[String], limit: usize) -> String {
    items
        .iter()
        .take(limit)
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

pub fn strip_frontmatter(raw: &str) -> String {
    parse_frontmatter(raw).body
}

pub fn infer_skill_name(path: &Path) -> String {
    path.parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- validate_name ---

    #[test]
    fn validate_name_valid() {
        assert!(validate_name("my-skill", "my-skill").is_empty());
    }

    #[test]
    fn validate_name_too_long() {
        let long = "a".repeat(MAX_SKILL_NAME_LEN + 1);
        let errors = validate_name(&long, &long);
        assert!(errors.iter().any(|e| e.contains("exceeds")));
    }

    #[test]
    fn validate_name_invalid_chars() {
        let errors = validate_name("My_Skill!", "My_Skill!");
        assert!(errors.iter().any(|e| e.contains("invalid characters")));
    }

    #[test]
    fn validate_name_leading_hyphen() {
        let errors = validate_name("-skill", "-skill");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("start or end with a hyphen"))
        );
    }

    #[test]
    fn validate_name_trailing_hyphen() {
        let errors = validate_name("skill-", "skill-");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("start or end with a hyphen"))
        );
    }

    #[test]
    fn validate_name_double_hyphen() {
        let errors = validate_name("my--skill", "my--skill");
        assert!(errors.iter().any(|e| e.contains("consecutive hyphens")));
    }

    #[test]
    fn validate_name_mismatch_parent_dir() {
        let errors = validate_name("foo", "bar");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("does not match parent directory"))
        );
    }

    // --- validate_description ---

    #[test]
    fn validate_description_empty() {
        let errors = validate_description("");
        assert!(errors.iter().any(|e| e.contains("required")));
    }

    #[test]
    fn validate_description_whitespace_only() {
        let errors = validate_description("   ");
        assert!(errors.iter().any(|e| e.contains("required")));
    }

    #[test]
    fn validate_description_too_long() {
        let long = "x".repeat(MAX_SKILL_DESC_LEN + 1);
        let errors = validate_description(&long);
        assert!(errors.iter().any(|e| e.contains("exceeds")));
    }

    #[test]
    fn validate_description_valid() {
        assert!(validate_description("A valid description").is_empty());
    }

    // --- validate_frontmatter_fields ---

    #[test]
    fn validate_frontmatter_fields_all_valid() {
        let keys: Vec<String> = vec!["name".into(), "description".into(), "license".into()];
        assert!(validate_frontmatter_fields(keys.iter()).is_empty());
    }

    #[test]
    fn validate_frontmatter_fields_unknown() {
        let keys: Vec<String> = vec!["name".into(), "unknown-field".into()];
        let errors = validate_frontmatter_fields(keys.iter());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("unknown-field"));
    }

    // --- parse_frontmatter ---

    #[test]
    fn parse_frontmatter_no_frontmatter() {
        let parsed = parse_frontmatter("Just a body\nwith lines");
        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, "Just a body\nwith lines");
        assert_eq!(parsed.error, None);
    }

    #[test]
    fn parse_frontmatter_valid() {
        let raw = "---\nname: my-skill\ndescription: A skill\n---\nBody content";
        let parsed = parse_frontmatter(raw);
        assert_eq!(
            frontmatter_string(&parsed.frontmatter, "name").as_deref(),
            Some("my-skill")
        );
        assert_eq!(
            frontmatter_string(&parsed.frontmatter, "description").as_deref(),
            Some("A skill")
        );
        assert_eq!(parsed.body, "Body content");
        assert_eq!(parsed.error, None);
    }

    #[test]
    fn parse_frontmatter_unclosed() {
        let raw = "---\nname: test\nno closing delimiter";
        let parsed = parse_frontmatter(raw);
        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, raw);
        assert_eq!(parsed.error.as_deref(), Some("unclosed YAML frontmatter"));
    }

    #[test]
    fn parse_frontmatter_comments_and_empty() {
        let raw = "---\n# comment\nname: test\n\n---\nBody";
        let parsed = parse_frontmatter(raw);
        assert_eq!(parsed.frontmatter.len(), 1);
        assert_eq!(
            frontmatter_string(&parsed.frontmatter, "name").as_deref(),
            Some("test")
        );
        assert_eq!(parsed.error, None);
    }

    #[test]
    fn parse_frontmatter_quoted_values() {
        let raw = "---\nname: \"my-skill\"\ndescription: 'A description'\n---\n";
        let parsed = parse_frontmatter(raw);
        assert_eq!(
            frontmatter_string(&parsed.frontmatter, "name").as_deref(),
            Some("my-skill")
        );
        assert_eq!(
            frontmatter_string(&parsed.frontmatter, "description").as_deref(),
            Some("A description")
        );
    }

    #[test]
    fn parse_frontmatter_empty_input() {
        let parsed = parse_frontmatter("");
        assert!(parsed.frontmatter.is_empty());
        assert!(parsed.body.is_empty());
        assert_eq!(parsed.error, None);
    }

    #[test]
    fn parse_frontmatter_preserves_structured_yaml_values() {
        let raw = "---\nname: my-skill\nmetadata:\n  owner: platform\n  tags:\n    - routing\n    - evaluation\nallowed-tools:\n  - read\n  - bash\ndisable-model-invocation: true\n---\nBody";
        let parsed = parse_frontmatter(raw);
        let owner_key = YamlValue::String("owner".to_string());

        assert_eq!(
            frontmatter_string(&parsed.frontmatter, "name").as_deref(),
            Some("my-skill")
        );
        assert_eq!(
            frontmatter_bool(&parsed.frontmatter, "disable-model-invocation"),
            Some(true)
        );
        assert_eq!(
            parsed
                .frontmatter
                .get("metadata")
                .and_then(YamlValue::as_mapping)
                .and_then(|metadata| metadata.get(&owner_key))
                .and_then(YamlValue::as_str),
            Some("platform")
        );
        assert_eq!(
            parsed
                .frontmatter
                .get("allowed-tools")
                .and_then(YamlValue::as_sequence)
                .map(|items| items
                    .iter()
                    .filter_map(YamlValue::as_str)
                    .collect::<Vec<_>>()),
            Some(vec!["read", "bash"])
        );
        assert_eq!(parsed.error, None);
    }

    #[test]
    fn parse_frontmatter_reports_invalid_yaml() {
        let parsed = parse_frontmatter("---\nname: [unterminated\n---\nBody");

        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, "Body");
        assert!(
            parsed
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("invalid YAML frontmatter:"))
        );
    }

    #[test]
    fn parse_skill_lineage_reads_provenance_metadata() {
        let raw = "---\nname: my-skill\ndescription: test\nmetadata:\n  provenance:\n    created-by-skill: skill-creator\n    created-by-revision: rev-a\n    last-improved-by-skill: pi-skills-doctor\n    last-improved-by-revision: managed-patch-v1\n    session-id: sess-7\n    intended-outcome: improve routing quality\n    baseline: manual authoring\n---\nBody";
        let parsed = parse_frontmatter(raw);
        let lineage = parse_skill_lineage(&parsed.frontmatter);
        assert_eq!(lineage.created_by_skill.as_deref(), Some("skill-creator"));
        assert_eq!(lineage.created_by_revision.as_deref(), Some("rev-a"));
        assert_eq!(
            lineage.last_improved_by_skill.as_deref(),
            Some("pi-skills-doctor")
        );
        assert_eq!(
            lineage.last_improved_by_revision.as_deref(),
            Some("managed-patch-v1")
        );
        assert_eq!(lineage.session_id.as_deref(), Some("sess-7"));
        assert_eq!(
            lineage.intended_outcome.as_deref(),
            Some("improve routing quality")
        );
        assert_eq!(lineage.baseline.as_deref(), Some("manual authoring"));
    }

    #[test]
    fn parse_skill_sections_collects_authoring_fields() {
        let sections = parse_skill_sections(
            "## Purpose\nHelp with release checks.\n\n## Use When\n- user asks for a release audit\n- user wants a go/no-go check\n\n## Not For\n- unrelated bug fixes\n\n## Trigger Examples\n- check deployment readiness\n\n## Anti-Trigger Examples\n- fix a production outage\n\n## Inputs\n- release branch\n\n## Output Contract\n- concise checklist\n\n## Success Criteria\n- zero missing critical blockers\n\n## Instructions\n1. inspect the release diff",
        );

        assert_eq!(
            sections.purpose,
            vec!["Help with release checks.".to_string()]
        );
        assert_eq!(sections.use_when.len(), 2);
        assert_eq!(sections.not_for, vec!["unrelated bug fixes".to_string()]);
        assert_eq!(
            sections.trigger_examples,
            vec!["check deployment readiness".to_string()]
        );
        assert_eq!(
            sections.anti_trigger_examples,
            vec!["fix a production outage".to_string()]
        );
        assert_eq!(sections.inputs, vec!["release branch".to_string()]);
        assert_eq!(
            sections.output_contract,
            vec!["concise checklist".to_string()]
        );
        assert_eq!(
            sections.success_criteria,
            vec!["zero missing critical blockers".to_string()]
        );
        assert_eq!(
            sections.instructions,
            vec!["inspect the release diff".to_string()]
        );
    }

    #[test]
    fn parse_skill_sections_accepts_paragraphs_and_normalizes_headings() {
        let sections = parse_skill_sections(
            "## Use-When:\nUse this when the user wants deeper code review.\n\n## Anti_Trigger_Examples\n- write a marketing email",
        );

        assert_eq!(
            sections.use_when,
            vec!["Use this when the user wants deeper code review.".to_string()]
        );
        assert_eq!(
            sections.anti_trigger_examples,
            vec!["write a marketing email".to_string()]
        );
    }

    #[test]
    fn parse_skill_sections_ignores_blank_list_items() {
        let sections = parse_skill_sections("## Trigger Examples\n- \n- actual request\n-   \n");

        assert_eq!(
            sections.trigger_examples,
            vec!["actual request".to_string()]
        );
    }

    // --- escape_xml ---

    #[test]
    fn escape_xml_all_special_chars() {
        assert_eq!(escape_xml("&"), "&amp;");
        assert_eq!(escape_xml("<"), "&lt;");
        assert_eq!(escape_xml(">"), "&gt;");
        assert_eq!(escape_xml("\""), "&quot;");
        assert_eq!(escape_xml("'"), "&apos;");
    }

    #[test]
    fn escape_xml_mixed() {
        assert_eq!(
            escape_xml("a <b> & 'c' \"d\""),
            "a &lt;b&gt; &amp; &apos;c&apos; &quot;d&quot;"
        );
    }

    #[test]
    fn escape_xml_no_special() {
        assert_eq!(escape_xml("plain text"), "plain text");
    }

    // --- expand_skill_command ---

    #[test]
    fn expand_skill_command_not_a_skill_command() {
        let skills = vec![];
        assert_eq!(expand_skill_command("hello world", &skills), "hello world");
    }

    #[test]
    fn expand_skill_command_missing_skill() {
        let skills = vec![];
        let result = expand_skill_command("/skill:nonexistent", &skills);
        assert_eq!(result, "/skill:nonexistent");
    }

    #[test]
    fn expand_skill_command_valid_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_file = skill_dir.join("SKILL.md");
        fs::write(
            &skill_file,
            "---\nname: my-skill\ndescription: test\n---\nDo something",
        )
        .unwrap();

        let skills = vec![Skill {
            name: "my-skill".to_string(),
            description: "test".to_string(),
            file_path: skill_file.clone(),
            base_dir: skill_dir.clone(),
            source: "test".to_string(),
            disable_model_invocation: false,
            sections: SkillSections::default(),
            lineage: SkillLineage::default(),
        }];

        let result = expand_skill_command("/skill:my-skill", &skills);
        assert!(result.contains("<skill name=\"my-skill\""));
        assert!(result.contains("Do something"));
    }

    #[test]
    fn expand_skill_command_with_args() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_file = skill_dir.join("SKILL.md");
        fs::write(
            &skill_file,
            "---\nname: my-skill\ndescription: test\n---\nBody",
        )
        .unwrap();

        let skills = vec![Skill {
            name: "my-skill".to_string(),
            description: "test".to_string(),
            file_path: skill_file.clone(),
            base_dir: skill_dir.clone(),
            source: "test".to_string(),
            disable_model_invocation: false,
            sections: SkillSections::default(),
            lineage: SkillLineage::default(),
        }];

        let result = expand_skill_command("/skill:my-skill extra args here", &skills);
        assert!(result.contains("extra args here"));
    }

    // --- format_skills_for_prompt ---

    #[test]
    fn format_skills_for_prompt_empty() {
        assert_eq!(format_skills_for_prompt(&[]), "");
    }

    #[test]
    fn format_skills_for_prompt_filters_disabled() {
        let skills = vec![Skill {
            name: "hidden".to_string(),
            description: "Hidden skill".to_string(),
            file_path: PathBuf::from("/tmp/hidden/SKILL.md"),
            base_dir: PathBuf::from("/tmp/hidden"),
            source: "test".to_string(),
            disable_model_invocation: true,
            sections: SkillSections::default(),
            lineage: SkillLineage::default(),
        }];
        assert_eq!(format_skills_for_prompt(&skills), "");
    }

    #[test]
    fn format_skills_for_prompt_multiple() {
        let skills = vec![
            Skill {
                name: "skill-a".to_string(),
                description: "First skill".to_string(),
                file_path: PathBuf::from("/tmp/skill-a/SKILL.md"),
                base_dir: PathBuf::from("/tmp/skill-a"),
                source: "test".to_string(),
                disable_model_invocation: false,
                sections: SkillSections {
                    use_when: vec!["the user asks for a release audit".to_string()],
                    not_for: vec!["incident response".to_string()],
                    trigger_examples: vec!["check deploy readiness".to_string()],
                    anti_trigger_examples: vec!["fix a production outage".to_string()],
                    ..SkillSections::default()
                },
                lineage: SkillLineage::default(),
            },
            Skill {
                name: "skill-b".to_string(),
                description: "Second skill".to_string(),
                file_path: PathBuf::from("/tmp/skill-b/SKILL.md"),
                base_dir: PathBuf::from("/tmp/skill-b"),
                source: "test".to_string(),
                disable_model_invocation: false,
                sections: SkillSections::default(),
                lineage: SkillLineage::default(),
            },
        ];
        let result = format_skills_for_prompt(&skills);
        assert!(result.contains("<name>skill-a</name>"));
        assert!(result.contains("<name>skill-b</name>"));
        assert!(result.contains("<available_skills>"));
        assert!(result.contains("</available_skills>"));
        assert!(result.contains("<use_when>the user asks for a release audit</use_when>"));
        assert!(result.contains("<not_for>incident response</not_for>"));
        assert!(result.contains("<trigger_examples>check deploy readiness</trigger_examples>"));
        assert!(
            result
                .contains("<anti_trigger_examples>fix a production outage</anti_trigger_examples>")
        );
    }

    // --- strip_frontmatter ---

    #[test]
    fn strip_frontmatter_returns_body() {
        let raw = "---\nname: test\n---\nThe body";
        assert_eq!(strip_frontmatter(raw), "The body");
    }

    // --- infer_skill_name ---

    #[test]
    fn infer_skill_name_normal_path() {
        let path = Path::new("/skills/my-skill/SKILL.md");
        assert_eq!(infer_skill_name(path), "my-skill");
    }

    #[test]
    fn infer_skill_name_root_path() {
        let path = Path::new("/SKILL.md");
        assert_eq!(infer_skill_name(path), "");
    }
}
