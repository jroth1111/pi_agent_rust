//! Skills module split from `resources` for clearer boundaries.
//!
//! - `schema`: SKILL.md parsing/validation + prompt rendering helpers
//! - `loader`: discovery + loading pipeline
//! - `resolver`: path classification helpers
//! - `installer`: install-source parsing helpers (npx/npm compatibility surface)

pub mod improvement;
pub mod installer;
pub mod loader;
pub mod resolver;
pub mod schema;

pub use improvement::{
    SKILL_AMENDMENT_ENTRY_TYPE, SKILL_OBSERVATION_ENTRY_TYPE, SkillDoctorFormat, SkillDoctorReport,
    SkillRunTracker, handle_skill_doctor, persist_skill_tracker,
};
pub use loader::{LoadSkillFileResult, LoadSkillsOptions, LoadSkillsResult, load_skills};
pub use resolver::is_under_path;
pub use schema::{
    ExplicitSkillInvocation, InputExpansion, MAX_SKILL_DESC_LEN, MAX_SKILL_NAME_LEN, Skill,
    expand_skill_command, expand_skill_command_with_trace, format_skills_for_prompt,
    parse_frontmatter, strip_frontmatter, validate_description, validate_frontmatter_fields,
    validate_name,
};
