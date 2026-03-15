use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::resources::{CollisionInfo, DiagnosticKind, ResourceDiagnostic};

use super::resolver::is_under_path;
use super::schema::{
    Skill, frontmatter_bool, frontmatter_string, infer_skill_name, parse_frontmatter,
    parse_skill_sections, validate_description, validate_frontmatter_fields, validate_name,
};

#[derive(Debug, Clone)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

#[derive(Debug, Clone)]
pub struct LoadSkillsOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub skill_paths: Vec<PathBuf>,
    pub include_defaults: bool,
}

#[derive(Debug, Clone)]
pub struct LoadSkillFileResult {
    pub skill: Option<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

#[allow(clippy::too_many_lines, clippy::items_after_statements)]
pub fn load_skills(options: LoadSkillsOptions) -> LoadSkillsResult {
    let mut skill_map: HashMap<String, Skill> = HashMap::new();
    let mut real_paths: HashSet<PathBuf> = HashSet::new();
    let mut diagnostics = Vec::new();
    let mut collisions = Vec::new();

    fn merge_skills(
        result: LoadSkillsResult,
        skill_map: &mut HashMap<String, Skill>,
        real_paths: &mut HashSet<PathBuf>,
        diagnostics: &mut Vec<ResourceDiagnostic>,
        collisions: &mut Vec<ResourceDiagnostic>,
    ) {
        diagnostics.extend(result.diagnostics);
        for skill in result.skills {
            let real_path =
                fs::canonicalize(&skill.file_path).unwrap_or_else(|_| skill.file_path.clone());
            if real_paths.contains(&real_path) {
                continue;
            }

            if let Some(existing) = skill_map.get(&skill.name) {
                collisions.push(ResourceDiagnostic {
                    kind: DiagnosticKind::Collision,
                    message: format!("name \"{}\" collision", skill.name),
                    path: skill.file_path.clone(),
                    collision: Some(CollisionInfo {
                        resource_type: "skill".to_string(),
                        name: skill.name.clone(),
                        winner_path: existing.file_path.clone(),
                        loser_path: skill.file_path.clone(),
                    }),
                });
            } else {
                real_paths.insert(real_path);
                skill_map.insert(skill.name.clone(), skill);
            }
        }
    }

    if options.include_defaults {
        merge_skills(
            load_skills_from_dir(options.agent_dir.join("skills"), "user".to_string(), true),
            &mut skill_map,
            &mut real_paths,
            &mut diagnostics,
            &mut collisions,
        );
        merge_skills(
            load_skills_from_dir(
                options.cwd.join(Config::project_dir()).join("skills"),
                "project".to_string(),
                true,
            ),
            &mut skill_map,
            &mut real_paths,
            &mut diagnostics,
            &mut collisions,
        );
    }

    for path in options.skill_paths {
        let resolved = path.clone();
        if !resolved.exists() {
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message: "skill path does not exist".to_string(),
                path: resolved,
                collision: None,
            });
            continue;
        }

        let source = if options.include_defaults {
            "path".to_string()
        } else if is_under_path(&resolved, &options.agent_dir.join("skills")) {
            "user".to_string()
        } else if is_under_path(
            &resolved,
            &options.cwd.join(Config::project_dir()).join("skills"),
        ) {
            "project".to_string()
        } else {
            "path".to_string()
        };

        match fs::metadata(&resolved) {
            Ok(meta) if meta.is_dir() => {
                merge_skills(
                    load_skills_from_dir(resolved, source, true),
                    &mut skill_map,
                    &mut real_paths,
                    &mut diagnostics,
                    &mut collisions,
                );
            }
            Ok(meta) if meta.is_file() && resolved.extension().is_some_and(|ext| ext == "md") => {
                let result = load_skill_from_file(&resolved, source);
                if let Some(skill) = result.skill {
                    merge_skills(
                        LoadSkillsResult {
                            skills: vec![skill],
                            diagnostics: result.diagnostics,
                        },
                        &mut skill_map,
                        &mut real_paths,
                        &mut diagnostics,
                        &mut collisions,
                    );
                } else {
                    diagnostics.extend(result.diagnostics);
                }
            }
            Ok(_) => {
                diagnostics.push(ResourceDiagnostic {
                    kind: DiagnosticKind::Warning,
                    message: "skill path is not a markdown file".to_string(),
                    path: resolved,
                    collision: None,
                });
            }
            Err(err) => diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message: format!("failed to read skill path: {err}"),
                path: resolved,
                collision: None,
            }),
        }
    }

    diagnostics.extend(collisions);

    let mut skills: Vec<Skill> = skill_map.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    LoadSkillsResult {
        skills,
        diagnostics,
    }
}

pub(crate) fn load_skills_from_dir(
    dir: PathBuf,
    source: String,
    include_root_files: bool,
) -> LoadSkillsResult {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    let mut visited_dirs = HashSet::new();
    let mut stack = vec![(dir, source, include_root_files)];

    while let Some((current_dir, current_source, current_include_root)) = stack.pop() {
        if !current_dir.exists() {
            continue;
        }

        let canonical_dir = fs::canonicalize(&current_dir).unwrap_or_else(|_| current_dir.clone());
        if !visited_dirs.insert(canonical_dir) {
            continue;
        }

        let Ok(entries) = fs::read_dir(&current_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();

            if file_name.starts_with('.') || file_name == "node_modules" {
                continue;
            }

            let full_path = entry.path();
            let file_type = entry.file_type();

            let (is_dir, is_file) = match file_type {
                Ok(ft) if ft.is_symlink() => match fs::metadata(&full_path) {
                    Ok(meta) => (meta.is_dir(), meta.is_file()),
                    Err(_) => continue,
                },
                Ok(ft) => (ft.is_dir(), ft.is_file()),
                Err(_) => continue,
            };

            if is_dir {
                stack.push((full_path, current_source.clone(), false));
                continue;
            }

            if !is_file {
                continue;
            }

            let is_root_md = current_include_root && file_name.ends_with(".md");
            let is_skill_md = !current_include_root && file_name == "SKILL.md";
            if !is_root_md && !is_skill_md {
                continue;
            }

            let result = load_skill_from_file(&full_path, current_source.clone());
            if let Some(skill) = result.skill {
                skills.push(skill);
            }
            diagnostics.extend(result.diagnostics);
        }
    }

    LoadSkillsResult {
        skills,
        diagnostics,
    }
}

pub(crate) fn load_skill_from_file(path: &Path, source: String) -> LoadSkillFileResult {
    let mut diagnostics = Vec::new();

    let Ok(raw) = fs::read_to_string(path) else {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: "failed to parse skill file".to_string(),
            path: path.to_path_buf(),
            collision: None,
        });
        return LoadSkillFileResult {
            skill: None,
            diagnostics,
        };
    };

    let parsed = parse_frontmatter(&raw);
    let frontmatter = &parsed.frontmatter;

    if let Some(error) = &parsed.error {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error.clone(),
            path: path.to_path_buf(),
            collision: None,
        });
    }

    let field_errors = validate_frontmatter_fields(frontmatter.keys());
    for error in field_errors {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error,
            path: path.to_path_buf(),
            collision: None,
        });
    }

    let description = frontmatter_string(frontmatter, "description").unwrap_or_default();
    let desc_errors = validate_description(&description);
    for error in desc_errors {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error,
            path: path.to_path_buf(),
            collision: None,
        });
    }

    if description.trim().is_empty() {
        return LoadSkillFileResult {
            skill: None,
            diagnostics,
        };
    }

    let base_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let parent_dir = infer_skill_name(path);
    let name = frontmatter_string(frontmatter, "name").unwrap_or_else(|| parent_dir.clone());

    let name_errors = validate_name(&name, &parent_dir);
    for error in name_errors {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error,
            path: path.to_path_buf(),
            collision: None,
        });
    }

    let disable_model_invocation =
        frontmatter_bool(frontmatter, "disable-model-invocation").unwrap_or(false);

    LoadSkillFileResult {
        skill: Some(Skill {
            name,
            description,
            file_path: path.to_path_buf(),
            base_dir,
            source,
            disable_model_invocation,
            sections: parse_skill_sections(&parsed.body),
        }),
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_skill_dir(base: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
        dir
    }

    // --- load_skill_from_file ---

    #[test]
    fn load_skill_from_file_valid() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = make_skill_dir(
            tmp.path(),
            "my-skill",
            "---\nname: my-skill\ndescription: A valid skill\n---\nBody",
        );
        let result = load_skill_from_file(&skill_dir.join("SKILL.md"), "test".to_string());
        assert!(result.skill.is_some());
        let skill = result.skill.unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "A valid skill");
        assert!(!skill.disable_model_invocation);
    }

    #[test]
    fn load_skill_from_file_missing_description() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = make_skill_dir(
            tmp.path(),
            "no-desc",
            "---\nname: no-desc\n---\nBody without description",
        );
        let result = load_skill_from_file(&skill_dir.join("SKILL.md"), "test".to_string());
        // No description => skill is None, diagnostic emitted
        assert!(result.skill.is_none());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("required"))
        );
    }

    #[test]
    fn load_skill_from_file_invalid_frontmatter_field() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = make_skill_dir(
            tmp.path(),
            "bad-field",
            "---\nname: bad-field\ndescription: Valid\nunknown-field: oops\n---\nBody",
        );
        let result = load_skill_from_file(&skill_dir.join("SKILL.md"), "test".to_string());
        assert!(result.skill.is_some());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown"))
        );
    }

    #[test]
    fn load_skill_from_file_reports_invalid_yaml_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = make_skill_dir(
            tmp.path(),
            "bad-yaml",
            "---\nname: [unterminated\ndescription: Broken\n---\nBody",
        );
        let result = load_skill_from_file(&skill_dir.join("SKILL.md"), "test".to_string());
        assert!(result.skill.is_none());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.starts_with("invalid YAML frontmatter:"))
        );
    }

    #[test]
    fn load_skill_from_file_disable_model_invocation() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = make_skill_dir(
            tmp.path(),
            "hidden",
            "---\nname: hidden\ndescription: Hidden skill\ndisable-model-invocation: true\n---\n",
        );
        let result = load_skill_from_file(&skill_dir.join("SKILL.md"), "test".to_string());
        assert!(result.skill.unwrap().disable_model_invocation);
    }

    #[test]
    fn load_skill_from_file_nonexistent() {
        let result = load_skill_from_file(
            std::path::Path::new("/nonexistent/path/SKILL.md"),
            "test".to_string(),
        );
        assert!(result.skill.is_none());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("failed"))
        );
    }

    #[test]
    fn load_skill_from_file_infers_name_from_dir() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = make_skill_dir(
            tmp.path(),
            "inferred",
            "---\ndescription: Name inferred from dir\n---\nBody",
        );
        let result = load_skill_from_file(&skill_dir.join("SKILL.md"), "test".to_string());
        let skill = result.skill.unwrap();
        assert_eq!(skill.name, "inferred");
    }

    // --- load_skills_from_dir ---

    #[test]
    fn load_skills_from_dir_nested() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        make_skill_dir(
            &root,
            "alpha",
            "---\nname: alpha\ndescription: Alpha\n---\n",
        );
        make_skill_dir(&root, "beta", "---\nname: beta\ndescription: Beta\n---\n");

        let result = load_skills_from_dir(root, "test".to_string(), false);
        assert_eq!(result.skills.len(), 2);
        let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn load_skills_from_dir_skips_hidden() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        make_skill_dir(
            &root,
            "visible",
            "---\nname: visible\ndescription: Visible\n---\n",
        );
        make_skill_dir(
            &root,
            ".hidden",
            "---\nname: hidden\ndescription: Hidden\n---\n",
        );

        let result = load_skills_from_dir(root, "test".to_string(), false);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "visible");
    }

    #[test]
    fn load_skills_from_dir_skips_node_modules() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        make_skill_dir(&root, "real", "---\nname: real\ndescription: Real\n---\n");
        make_skill_dir(
            &root,
            "node_modules",
            "---\nname: node-modules\ndescription: Should skip\n---\n",
        );

        let result = load_skills_from_dir(root, "test".to_string(), false);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "real");
    }

    #[test]
    fn load_skills_from_dir_nonexistent() {
        let result = load_skills_from_dir(
            PathBuf::from("/nonexistent/path"),
            "test".to_string(),
            false,
        );
        assert!(result.skills.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn load_skills_from_dir_root_md_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        // With include_root_files=true, .md files in the root dir are loaded
        fs::write(
            root.join("quick.md"),
            "---\ndescription: Quick skill\n---\nQuick body",
        )
        .unwrap();

        let result = load_skills_from_dir(root, "test".to_string(), true);
        assert_eq!(result.skills.len(), 1);
    }

    // --- load_skills (integration) ---

    #[test]
    fn load_skills_explicit_path_file() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = make_skill_dir(
            tmp.path(),
            "explicit",
            "---\nname: explicit\ndescription: Explicit path skill\n---\n",
        );

        let result = load_skills(LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: tmp.path().join("agent"),
            skill_paths: vec![skill_dir.join("SKILL.md")],
            include_defaults: false,
        });
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "explicit");
    }

    #[test]
    fn load_skills_nonexistent_path_diagnostic() {
        let tmp = TempDir::new().unwrap();
        let result = load_skills(LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: tmp.path().join("agent"),
            skill_paths: vec![PathBuf::from("/nonexistent/skill/path")],
            include_defaults: false,
        });
        assert!(result.skills.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("does not exist"))
        );
    }

    #[test]
    fn load_skills_collision_detection() {
        let tmp = TempDir::new().unwrap();
        let dir_a = make_skill_dir(
            tmp.path(),
            "same-name-a",
            "---\nname: collider\ndescription: First\n---\n",
        );
        let dir_b = make_skill_dir(
            tmp.path(),
            "same-name-b",
            "---\nname: collider\ndescription: Second\n---\n",
        );

        let result = load_skills(LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: tmp.path().join("agent"),
            skill_paths: vec![dir_a.join("SKILL.md"), dir_b.join("SKILL.md")],
            include_defaults: false,
        });
        // Only first wins
        assert_eq!(result.skills.len(), 1);
        // Collision diagnostic emitted
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("collision"))
        );
    }

    #[test]
    fn load_skills_results_sorted_by_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        make_skill_dir(&root, "zebra", "---\nname: zebra\ndescription: Z\n---\n");
        make_skill_dir(&root, "alpha", "---\nname: alpha\ndescription: A\n---\n");

        let result = load_skills(LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: tmp.path().join("agent"),
            skill_paths: vec![root],
            include_defaults: false,
        });
        assert_eq!(result.skills.len(), 2);
        assert_eq!(result.skills[0].name, "alpha");
        assert_eq!(result.skills[1].name, "zebra");
    }
}
