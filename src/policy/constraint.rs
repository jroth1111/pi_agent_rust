//! Constraint system for controlling file modification scope.
//!
//! This module provides a hypervisor-style constraint system for auditing
//! file changes before they are committed. Constraints define invariants
//! that must be upheld by any diff.
//!
//! # Example
//!
//! ```
//! use pi::policy::{ConstraintSet, Invariant, DiffSummary};
//!
//! let mut constraints = ConstraintSet::new();
//! constraints.add_invariant(Invariant::no_touch(".env*"));
//! constraints.add_invariant(Invariant::max_files_changed(10));
//!
//! let diff = DiffSummary {
//!     changed: vec!["src/main.rs".into()],
//!     added: vec![],
//!     removed: vec![],
//! };
//!
//! constraints.audit_diff(&diff).expect("diff passes constraints");
//! ```

use std::path::Path;

use glob::Pattern;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A summary of file changes to be audited.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffSummary {
    /// Files that were modified.
    pub changed: Vec<String>,
    /// Files that were newly created.
    pub added: Vec<String>,
    /// Files that were deleted.
    pub removed: Vec<String>,
}

impl DiffSummary {
    /// Creates a new empty diff summary.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a diff summary from the given changes.
    #[must_use]
    pub const fn from_changes(
        changed: Vec<String>,
        added: Vec<String>,
        removed: Vec<String>,
    ) -> Self {
        Self {
            changed,
            added,
            removed,
        }
    }

    /// Returns the total number of files affected.
    #[must_use]
    pub fn total_files(&self) -> usize {
        self.changed.len() + self.added.len() + self.removed.len()
    }

    /// Returns all affected file paths.
    #[must_use]
    pub fn all_files(&self) -> Vec<&str> {
        self.changed
            .iter()
            .chain(self.added.iter())
            .chain(self.removed.iter())
            .map(String::as_str)
            .collect()
    }

    /// Returns `true` if there are no changes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

/// An invariant that must be upheld by file changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Invariant {
    /// Cannot modify files matching the pattern.
    NoTouch {
        /// Glob pattern for protected files.
        pattern: String,
    },

    /// Must modify at least one file matching the pattern.
    MustTouch {
        /// Glob pattern for required files.
        pattern: String,
    },

    /// Maximum number of files that can be changed.
    MaxFilesChanged {
        /// Maximum allowed changed files.
        max: usize,
    },

    /// Cannot create new files (optionally filtered by pattern).
    NoNewFiles {
        /// Optional glob pattern. If set, only files matching this pattern are restricted.
        #[serde(skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
}

impl Invariant {
    /// Creates a `NoTouch` invariant with the given glob pattern.
    #[must_use]
    pub fn no_touch(pattern: impl Into<String>) -> Self {
        Self::NoTouch {
            pattern: pattern.into(),
        }
    }

    /// Creates a `MustTouch` invariant with the given glob pattern.
    #[must_use]
    pub fn must_touch(pattern: impl Into<String>) -> Self {
        Self::MustTouch {
            pattern: pattern.into(),
        }
    }

    /// Creates a `MaxFilesChanged` invariant with the given limit.
    #[must_use]
    pub const fn max_files_changed(max: usize) -> Self {
        Self::MaxFilesChanged { max }
    }

    /// Creates a `NoNewFiles` invariant for all files.
    #[must_use]
    pub const fn no_new_files() -> Self {
        Self::NoNewFiles { pattern: None }
    }

    /// Creates a `NoNewFiles` invariant for files matching the pattern.
    #[must_use]
    pub fn no_new_files_matching(pattern: impl Into<String>) -> Self {
        Self::NoNewFiles {
            pattern: Some(pattern.into()),
        }
    }

    /// Checks if a path matches a glob pattern.
    fn matches_glob(path: &str, pattern: &str) -> bool {
        // Try to compile the pattern, fall back to simple prefix/suffix matching
        match Pattern::new(pattern) {
            Ok(glob) => glob.matches(path),
            Err(_) => {
                // Fallback: simple contains/starts_with/ends_with matching
                if pattern.starts_with('*') && pattern.ends_with('*') {
                    let middle = &pattern[1..pattern.len() - 1];
                    path.contains(middle)
                } else if pattern.starts_with('*') {
                    path.ends_with(&pattern[1..])
                } else if pattern.ends_with('*') {
                    path.starts_with(&pattern[..pattern.len() - 1])
                } else {
                    path == pattern
                }
            }
        }
    }

    /// Validates a single file against this invariant.
    ///
    /// Returns `Ok(())` if the file passes, or an error describing the violation.
    fn check_file(&self, path: &str, operation: FileOperation) -> Result<()> {
        match self {
            Self::NoTouch { pattern } => {
                if Self::matches_glob(path, pattern) {
                    return Err(Error::Validation(format!(
                        "NoTouch constraint violated: '{path}' matches protected pattern '{pattern}'"
                    )));
                }
            }
            Self::NoNewFiles { pattern } => {
                if operation == FileOperation::Add {
                    if let Some(p) = pattern {
                        if Self::matches_glob(path, p) {
                            return Err(Error::Validation(format!(
                                "NoNewFiles constraint violated: '{path}' matches pattern '{p}'"
                            )));
                        }
                    } else {
                        return Err(Error::Validation(format!(
                            "NoNewFiles constraint violated: '{path}' is a new file"
                        )));
                    }
                }
            }
            Self::MaxFilesChanged { .. } | Self::MustTouch { .. } => {
                // These are checked at the aggregate level
            }
        }
        Ok(())
    }

    /// Checks the entire diff summary against this invariant.
    fn check_diff(&self, diff: &DiffSummary) -> Result<()> {
        // Check individual files
        for path in &diff.changed {
            self.check_file(path, FileOperation::Change)?;
        }
        for path in &diff.added {
            self.check_file(path, FileOperation::Add)?;
        }
        for path in &diff.removed {
            self.check_file(path, FileOperation::Remove)?;
        }

        // Check aggregate constraints
        match self {
            Self::MaxFilesChanged { max } => {
                let total = diff.total_files();
                if total > *max {
                    return Err(Error::Validation(format!(
                        "MaxFilesChanged constraint violated: {total} files changed exceeds limit of {max}"
                    )));
                }
            }
            Self::MustTouch { pattern } => {
                let all_files = diff.all_files();
                let touches_pattern = all_files.iter().any(|p| Self::matches_glob(p, pattern));
                if !touches_pattern {
                    return Err(Error::Validation(format!(
                        "MustTouch constraint violated: no file matching '{pattern}' was modified"
                    )));
                }
            }
            Self::NoTouch { .. } | Self::NoNewFiles { .. } => {}
        }

        Ok(())
    }
}

/// The type of file operation being performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    /// Modifying an existing file.
    Change,
    /// Creating a new file.
    Add,
    /// Removing a file.
    Remove,
}

/// A set of constraints that must all be satisfied.
///
/// Constraints are checked in order, and all must pass for the
/// diff to be accepted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConstraintSet {
    /// The invariants that must be upheld.
    #[serde(default)]
    pub invariants: Vec<Invariant>,
}

impl ConstraintSet {
    /// Creates a new empty constraint set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an invariant to the constraint set.
    pub fn add_invariant(&mut self, invariant: Invariant) {
        self.invariants.push(invariant);
    }

    /// Creates a constraint set with the given invariants.
    #[must_use]
    pub const fn with_invariants(invariants: Vec<Invariant>) -> Self {
        Self { invariants }
    }

    /// Parses a TOML configuration into a constraint set.
    ///
    /// # Errors
    ///
    /// Returns an error if the TOML is malformed.
    pub fn from_toml(toml: &str) -> std::result::Result<Self, toml::de::Error> {
        toml::from_str(toml)
    }

    /// Parses a JSON configuration into a constraint set.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is malformed.
    pub fn from_json(json: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Converts the constraint set to a TOML string.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_toml(&self) -> std::result::Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Converts the constraint set to a JSON string.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> std::result::Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Audits a diff against all constraints.
    ///
    /// # Errors
    ///
    /// Returns an error if any invariant is violated.
    pub fn audit_diff(&self, diff: &DiffSummary) -> Result<()> {
        for invariant in &self.invariants {
            invariant.check_diff(diff)?;
        }
        Ok(())
    }

    /// Returns the number of invariants in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.invariants.len()
    }

    /// Returns `true` if there are no invariants.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.invariants.is_empty()
    }

    /// Checks if a single file path would violate any constraints.
    ///
    /// This is useful for pre-flight checks before generating a diff.
    ///
    /// # Errors
    ///
    /// Returns an error if the path would violate any invariant.
    pub fn check_path(&self, path: &Path, operation: FileOperation) -> Result<()> {
        let path_str = path.to_string_lossy();
        for invariant in &self.invariants {
            invariant.check_file(&path_str, operation)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_summary_total() {
        let diff = DiffSummary {
            changed: vec!["a.rs".into(), "b.rs".into()],
            added: vec!["c.rs".into()],
            removed: vec!["d.rs".into()],
        };
        assert_eq!(diff.total_files(), 4);
    }

    #[test]
    fn test_diff_summary_all_files() {
        let diff = DiffSummary {
            changed: vec!["a.rs".into()],
            added: vec!["b.rs".into()],
            removed: vec!["c.rs".into()],
        };
        let all = diff.all_files();
        assert_eq!(all.len(), 3);
        assert!(all.contains(&"a.rs"));
        assert!(all.contains(&"b.rs"));
        assert!(all.contains(&"c.rs"));
    }

    #[test]
    fn test_invariant_no_touch() {
        let inv = Invariant::no_touch("*.env");

        // Matches pattern
        assert!(inv.check_file(".env", FileOperation::Change).is_err());
        assert!(inv.check_file("test.env", FileOperation::Change).is_err());
        assert!(inv.check_file("prod.env", FileOperation::Change).is_err());

        // Doesn't match
        assert!(inv.check_file("main.rs", FileOperation::Change).is_ok());
        assert!(inv.check_file("env.txt", FileOperation::Change).is_ok());
    }

    #[test]
    fn test_invariant_must_touch() {
        let inv = Invariant::must_touch("Cargo.toml");

        // Touches pattern
        let good_diff = DiffSummary {
            changed: vec!["Cargo.toml".into(), "src/main.rs".into()],
            added: vec![],
            removed: vec![],
        };
        assert!(inv.check_diff(&good_diff).is_ok());

        // Doesn't touch pattern
        let bad_diff = DiffSummary {
            changed: vec!["src/main.rs".into()],
            added: vec![],
            removed: vec![],
        };
        assert!(inv.check_diff(&bad_diff).is_err());
    }

    #[test]
    fn test_invariant_max_files_changed() {
        let inv = Invariant::max_files_changed(3);

        // Under limit
        let good_diff = DiffSummary {
            changed: vec!["a.rs".into(), "b.rs".into()],
            added: vec!["c.rs".into()],
            removed: vec![],
        };
        assert!(inv.check_diff(&good_diff).is_ok());

        // At limit
        let at_limit = DiffSummary {
            changed: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
            added: vec![],
            removed: vec![],
        };
        assert!(inv.check_diff(&at_limit).is_ok());

        // Over limit
        let bad_diff = DiffSummary {
            changed: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
            added: vec!["d.rs".into()],
            removed: vec![],
        };
        assert!(inv.check_diff(&bad_diff).is_err());
    }

    #[test]
    fn test_invariant_no_new_files() {
        let inv = Invariant::no_new_files();

        // No new files
        let good_diff = DiffSummary {
            changed: vec!["a.rs".into()],
            added: vec![],
            removed: vec![],
        };
        assert!(inv.check_diff(&good_diff).is_ok());

        // Has new files
        let bad_diff = DiffSummary {
            changed: vec![],
            added: vec!["new.rs".into()],
            removed: vec![],
        };
        assert!(inv.check_diff(&bad_diff).is_err());
    }

    #[test]
    fn test_invariant_no_new_files_with_pattern() {
        let inv = Invariant::no_new_files_matching("*.secret");

        // New file doesn't match pattern
        let good_diff = DiffSummary {
            changed: vec![],
            added: vec!["normal.txt".into()],
            removed: vec![],
        };
        assert!(inv.check_diff(&good_diff).is_ok());

        // New file matches pattern
        let bad_diff = DiffSummary {
            changed: vec![],
            added: vec!["api.secret".into()],
            removed: vec![],
        };
        assert!(inv.check_diff(&bad_diff).is_err());
    }

    #[test]
    fn test_constraint_set_audit() {
        let mut set = ConstraintSet::new();
        set.add_invariant(Invariant::no_touch("*.env"));
        set.add_invariant(Invariant::max_files_changed(5));

        // Passes all constraints
        let good_diff = DiffSummary {
            changed: vec!["src/main.rs".into()],
            added: vec!["src/lib.rs".into()],
            removed: vec![],
        };
        assert!(set.audit_diff(&good_diff).is_ok());

        // Violates NoTouch
        let bad_diff1 = DiffSummary {
            changed: vec![".env".into()],
            added: vec![],
            removed: vec![],
        };
        assert!(set.audit_diff(&bad_diff1).is_err());

        // Violates MaxFilesChanged
        let bad_diff2 = DiffSummary {
            changed: (0..6).map(|i| format!("file{i}.rs")).collect(),
            added: vec![],
            removed: vec![],
        };
        assert!(set.audit_diff(&bad_diff2).is_err());
    }

    #[test]
    fn test_constraint_set_from_toml() {
        let toml = r#"
[[invariants]]
type = "no_touch"
pattern = "*.env"

[[invariants]]
type = "max_files_changed"
max = 10
"#;
        let set = ConstraintSet::from_toml(toml).expect("parse toml");
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_constraint_set_from_json() {
        let json = r#"{
            "invariants": [
                {"type": "no_touch", "pattern": "*.env"},
                {"type": "max_files_changed", "max": 10}
            ]
        }"#;
        let set = ConstraintSet::from_json(json).expect("parse json");
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_constraint_set_roundtrip_toml() {
        let mut set = ConstraintSet::new();
        set.add_invariant(Invariant::no_touch("*.env"));
        set.add_invariant(Invariant::max_files_changed(10));

        let toml = set.to_toml().expect("serialize");
        let parsed = ConstraintSet::from_toml(&toml).expect("parse");
        assert_eq!(set, parsed);
    }

    #[test]
    fn test_constraint_set_roundtrip_json() {
        let mut set = ConstraintSet::new();
        set.add_invariant(Invariant::no_touch("*.env"));
        set.add_invariant(Invariant::must_touch("Cargo.toml"));

        let json = set.to_json().expect("serialize");
        let parsed = ConstraintSet::from_json(&json).expect("parse");
        assert_eq!(set, parsed);
    }

    #[test]
    fn test_check_path() {
        let mut set = ConstraintSet::new();
        set.add_invariant(Invariant::no_touch("*.env"));

        assert!(
            set.check_path(Path::new("main.rs"), FileOperation::Change)
                .is_ok()
        );
        assert!(
            set.check_path(Path::new(".env"), FileOperation::Change)
                .is_err()
        );
    }

    #[test]
    fn test_empty_constraint_set() {
        let set = ConstraintSet::new();
        let diff = DiffSummary {
            changed: vec!["anything".into()],
            added: vec![],
            removed: vec![],
        };
        assert!(set.audit_diff(&diff).is_ok());
    }
}
