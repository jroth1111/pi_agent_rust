//! Installer-side helpers for skill source parsing.
//!
//! This module provides a compatibility surface for npm/npx-oriented workflows
//! without coupling installer behavior to resource loading.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillInstallSource {
    NpmPackage(String),
    LocalPath(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInstallRequest {
    pub source: SkillInstallSource,
    pub destination_dir: Option<PathBuf>,
}

impl SkillInstallRequest {
    pub fn from_source(source: &str, cwd: &Path, destination_dir: Option<PathBuf>) -> Self {
        Self {
            source: parse_install_source(source, cwd),
            destination_dir,
        }
    }
}

pub fn parse_install_source(source: &str, cwd: &Path) -> SkillInstallSource {
    let trimmed = source.trim();
    if let Some(pkg) = trimmed.strip_prefix("npm:") {
        return SkillInstallSource::NpmPackage(pkg.trim().to_string());
    }
    if let Some(pkg) = trimmed.strip_prefix("npx:") {
        return SkillInstallSource::NpmPackage(pkg.trim().to_string());
    }

    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        SkillInstallSource::LocalPath(path)
    } else {
        SkillInstallSource::LocalPath(cwd.join(path))
    }
}

pub fn default_install_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("skills")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_install_source_npm_prefix() {
        let source = parse_install_source("npm:my-package", Path::new("/cwd"));
        assert_eq!(
            source,
            SkillInstallSource::NpmPackage("my-package".to_string())
        );
    }

    #[test]
    fn parse_install_source_npx_prefix() {
        let source = parse_install_source("npx:my-tool", Path::new("/cwd"));
        assert_eq!(
            source,
            SkillInstallSource::NpmPackage("my-tool".to_string())
        );
    }

    #[test]
    fn parse_install_source_absolute_path() {
        let source = parse_install_source("/usr/local/skills/my-skill", Path::new("/cwd"));
        assert_eq!(
            source,
            SkillInstallSource::LocalPath(PathBuf::from("/usr/local/skills/my-skill"))
        );
    }

    #[test]
    fn parse_install_source_relative_path() {
        let source = parse_install_source("./my-skill", Path::new("/cwd"));
        assert_eq!(
            source,
            SkillInstallSource::LocalPath(PathBuf::from("/cwd/./my-skill"))
        );
    }

    #[test]
    fn parse_install_source_trims_whitespace() {
        let source = parse_install_source("  npm:trimmed  ", Path::new("/cwd"));
        assert_eq!(
            source,
            SkillInstallSource::NpmPackage("trimmed".to_string())
        );
    }

    #[test]
    fn install_request_from_source() {
        let req = SkillInstallRequest::from_source("npm:pkg", Path::new("/cwd"), None);
        assert_eq!(
            req.source,
            SkillInstallSource::NpmPackage("pkg".to_string())
        );
        assert!(req.destination_dir.is_none());
    }

    #[test]
    fn install_request_with_destination() {
        let dest = PathBuf::from("/custom/dir");
        let req =
            SkillInstallRequest::from_source("npm:pkg", Path::new("/cwd"), Some(dest.clone()));
        assert_eq!(req.destination_dir, Some(dest));
    }

    #[test]
    fn default_install_dir_appends_skills() {
        let dir = default_install_dir(Path::new("/home/user/.agent"));
        assert_eq!(dir, PathBuf::from("/home/user/.agent/skills"));
    }
}
