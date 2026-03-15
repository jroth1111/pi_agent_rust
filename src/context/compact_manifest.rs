use std::fmt::Write as _;

#[derive(Debug, Clone, Default)]
pub struct CompactManifest {
    sections: Vec<CompactManifestSection>,
}

#[derive(Debug, Clone)]
enum CompactManifestSection {
    Fields {
        name: String,
        rows: Vec<(String, String)>,
    },
    Table {
        name: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

impl CompactManifest {
    pub fn push_fields(&mut self, name: impl Into<String>, rows: Vec<(String, String)>) {
        if rows.is_empty() {
            return;
        }
        self.sections.push(CompactManifestSection::Fields {
            name: name.into(),
            rows,
        });
    }

    pub fn push_table(
        &mut self,
        name: impl Into<String>,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    ) {
        if columns.is_empty() || rows.is_empty() {
            return;
        }
        self.sections.push(CompactManifestSection::Table {
            name: name.into(),
            columns,
            rows,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    pub fn render(&self) -> String {
        let mut out = String::new();

        for (section_idx, section) in self.sections.iter().enumerate() {
            if section_idx > 0 {
                out.push_str("\n\n");
            }

            match section {
                CompactManifestSection::Fields { name, rows } => {
                    let _ = write!(&mut out, "@fields {name}");
                    for (key, value) in rows {
                        let _ = write!(&mut out, "\n{key}={value}");
                    }
                }
                CompactManifestSection::Table {
                    name,
                    columns,
                    rows,
                } => {
                    let _ = write!(&mut out, "@table {name} {}", columns.join("|"));
                    for row in rows {
                        let _ = write!(&mut out, "\n{}", row.join("|"));
                    }
                }
            }
        }

        out
    }
}

pub fn normalize_compact_value(value: &str, max_chars: usize) -> String {
    let mut compact = value
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    compact = compact.replace('|', "/");

    if compact.is_empty() {
        return "none".to_string();
    }

    if compact.chars().count() <= max_chars {
        return compact;
    }

    let trimmed = compact
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    format!("{trimmed}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_manifest_renders_fields_and_tables() {
        let mut manifest = CompactManifest::default();
        manifest.push_fields("runtime", vec![("task".to_string(), "task-1".to_string())]);
        manifest.push_table(
            "retrieval",
            vec!["path".to_string(), "outline".to_string()],
            vec![vec!["src/lib.rs".to_string(), "fn main".to_string()]],
        );

        let rendered = manifest.render();
        assert!(rendered.contains("@fields runtime"));
        assert!(rendered.contains("task=task-1"));
        assert!(rendered.contains("@table retrieval path|outline"));
        assert!(rendered.contains("src/lib.rs|fn main"));
    }

    #[test]
    fn normalize_compact_value_collapses_whitespace_and_limits_length() {
        let value = normalize_compact_value(" one\n two \t three | four ", 14);
        assert_eq!(value, "one two thr...");
    }
}
