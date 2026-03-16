use super::loader::{LoadSkillsOptions, load_skills};
use super::schema::{
    ExplicitSkillInvocation, Skill, SkillLineage, SkillSections, legacy_skill_id,
    parse_frontmatter, parse_skill_lineage, parse_skill_sections, strip_frontmatter,
};
use crate::agent::AgentEvent;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::extensions::sha256_hex_standalone;
use crate::session::Session;
use crate::tools::ToolOutput;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const SKILL_OBSERVATION_ENTRY_TYPE: &str = "skill/observation.v1";
pub const SKILL_AMENDMENT_ENTRY_TYPE: &str = "skill/amendment.v1";
pub const SKILL_FEEDBACK_ENTRY_TYPE: &str = "skill/feedback.v1";
const SKILL_OBSERVATION_LEDGER: &str = "skill-observations.jsonl";
const SKILL_AMENDMENT_LEDGER: &str = "skill-amendments.jsonl";
const SKILL_FEEDBACK_LEDGER: &str = "skill-feedback.jsonl";
const GUARDRAIL_BLOCK_BEGIN: &str = "<!-- PI-SKILL-GUARDRAILS:BEGIN -->";
const GUARDRAIL_BLOCK_END: &str = "<!-- PI-SKILL-GUARDRAILS:END -->";
const ROUTING_BLOCK_BEGIN: &str = "<!-- PI-SKILL-ROUTING:BEGIN -->";
const ROUTING_BLOCK_END: &str = "<!-- PI-SKILL-ROUTING:END -->";
const PI_SKILLS_DOCTOR_NAME: &str = "pi-skills-doctor";
const PI_SKILLS_DOCTOR_REVISION: &str = "managed-patch-v1";
const LOW_FEEDBACK_THRESHOLD: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillActivationSource {
    SlashCommand,
    ReadTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillRunOutcome {
    Success,
    ToolFailure,
    AgentFailure,
    Aborted,
}

impl SkillRunOutcome {
    fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillToolFailure {
    pub tool_name: String,
    pub signature: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillObservation {
    pub version: u8,
    pub recorded_at_utc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub skill_id: String,
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub skill_digest: String,
    pub activation_source: SkillActivationSource,
    pub task_preview: String,
    pub outcome: SkillRunOutcome,
    #[serde(default, skip_serializing_if = "SkillLineage::is_empty")]
    pub lineage: SkillLineage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_failures: Vec<SkillToolFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRoutingHints {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub use_when: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_for: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_examples: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anti_trigger_examples: Vec<String>,
}

impl SkillRoutingHints {
    fn is_empty(&self) -> bool {
        self.use_when.is_empty()
            && self.not_for.is_empty()
            && self.trigger_examples.is_empty()
            && self.anti_trigger_examples.is_empty()
    }

    fn normalize(&mut self) {
        normalize_items(&mut self.use_when);
        normalize_items(&mut self.not_for);
        normalize_items(&mut self.trigger_examples);
        normalize_items(&mut self.anti_trigger_examples);
    }

    fn from_sections(sections: &SkillSections) -> Self {
        let mut hints = Self {
            use_when: sections.use_when.clone(),
            not_for: sections.not_for.clone(),
            trigger_examples: sections.trigger_examples.clone(),
            anti_trigger_examples: sections.anti_trigger_examples.clone(),
        };
        hints.normalize();
        hints
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillAmendment {
    pub version: u8,
    pub applied_at_utc: String,
    #[serde(default)]
    pub skill_id: String,
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub previous_digest: String,
    pub new_digest: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_guardrails: Vec<String>,
    pub guardrails: Vec<String>,
    #[serde(default, skip_serializing_if = "SkillRoutingHints::is_empty")]
    pub previous_routing_hints: SkillRoutingHints,
    #[serde(default, skip_serializing_if = "SkillRoutingHints::is_empty")]
    pub routing_hints: SkillRoutingHints,
    #[serde(default, skip_serializing_if = "SkillLineage::is_empty")]
    pub lineage: SkillLineage,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillFeedback {
    pub version: u8,
    pub recorded_at_utc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub skill_id: String,
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub skill_digest: String,
    #[serde(default, skip_serializing_if = "SkillLineage::is_empty")]
    pub lineage: SkillLineage,
    pub rating: u8,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillHealthStatus {
    Healthy,
    NeedsAmendment,
    Improved,
    Regressed,
    PendingData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSuccessCriteria {
    pub min_sample_size: usize,
    pub min_success_rate: f64,
    pub max_consecutive_failures: usize,
    pub min_post_amend_runs: usize,
    pub min_improvement_delta: f64,
    pub min_feedback_sample_size: usize,
    pub min_average_feedback_score: f64,
    pub min_feedback_improvement_delta: f64,
}

impl Default for SkillSuccessCriteria {
    fn default() -> Self {
        Self {
            min_sample_size: 3,
            min_success_rate: 0.8,
            max_consecutive_failures: 2,
            min_post_amend_runs: 3,
            min_improvement_delta: 0.15,
            min_feedback_sample_size: 2,
            min_average_feedback_score: 3.5,
            min_feedback_improvement_delta: 0.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillHealthReport {
    pub skill_id: String,
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub latest_digest: String,
    pub current_digest: String,
    pub run_count: usize,
    pub success_rate: f64,
    pub consecutive_failures: usize,
    pub feedback_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_feedback_score: Option<f64>,
    pub low_feedback_count: usize,
    pub status: SkillHealthStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed_guardrails: Vec<String>,
    #[serde(default, skip_serializing_if = "SkillRoutingHints::is_empty")]
    pub proposed_routing_hints: SkillRoutingHints,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_success_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_success_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_average_feedback_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_average_feedback_score: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillProducerRole {
    Creator,
    Improver,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillProducerReport {
    pub producer_skill_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_revision: Option<String>,
    pub role: SkillProducerRole,
    pub descendant_skill_count: usize,
    pub observed_descendant_skill_count: usize,
    pub unobserved_descendant_skill_count: usize,
    pub orphaned_descendant_skill_count: usize,
    pub effective_descendant_skill_count: usize,
    pub pending_descendant_skill_count: usize,
    pub needs_amendment_descendant_skill_count: usize,
    pub regressed_descendant_skill_count: usize,
    pub effective_descendant_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_descendant_success_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_descendant_feedback_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDoctorReport {
    pub criteria: SkillSuccessCriteria,
    pub observation_count: usize,
    pub feedback_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillHealthReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub producers: Vec<SkillProducerReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_amendments: Vec<SkillAmendment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDoctorFormat {
    Text,
    Json,
}

impl SkillDoctorFormat {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(Error::validation(format!(
                "Unsupported skills doctor format: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct TrackedSkill {
    skill_id: String,
    name: String,
    path: PathBuf,
    digest: String,
    lineage: SkillLineage,
}

#[derive(Debug, Clone)]
struct ActivatedSkill {
    skill: TrackedSkill,
    source: SkillActivationSource,
}

#[derive(Debug, Clone)]
struct SkillFeedbackTarget {
    skill_id: String,
    name: String,
    path: PathBuf,
    digest: String,
    lineage: SkillLineage,
}

#[derive(Debug, Clone, Copy)]
struct RevisionEvaluation {
    status: SkillHealthStatus,
    previous_success_rate: Option<f64>,
    latest_success_rate: Option<f64>,
    previous_average_feedback_score: Option<f64>,
    latest_average_feedback_score: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SkillProducerKey {
    role: SkillProducerRole,
    name: String,
    revision: Option<String>,
}

#[derive(Debug, Clone)]
struct SkillEvidenceSnapshot {
    recorded_at_utc: String,
    skill_name: String,
    lineage: SkillLineage,
}

#[derive(Debug, Clone)]
struct ProducerDescendant {
    skill_id: String,
    skill_name: String,
    report: Option<SkillHealthReport>,
    orphaned: bool,
}

#[derive(Debug, Clone)]
pub struct SkillRunTracker {
    cwd: PathBuf,
    task_preview: String,
    session_id: Option<String>,
    skills_by_path: HashMap<PathBuf, TrackedSkill>,
    activations: BTreeMap<String, ActivatedSkill>,
    pending_skill_reads: HashMap<String, PathBuf>,
    tool_failures: Vec<SkillToolFailure>,
    agent_error: Option<String>,
}

impl SkillRunTracker {
    pub fn new(
        cwd: PathBuf,
        task_text: &str,
        skills: &[Skill],
        explicit_skill: Option<ExplicitSkillInvocation>,
    ) -> Self {
        let mut skills_by_path = HashMap::new();
        for skill in skills {
            let canonical = canonicalize_with_fallback(&skill.file_path);
            skills_by_path.insert(
                canonical,
                TrackedSkill {
                    skill_id: skill.skill_id.clone(),
                    name: skill.name.clone(),
                    path: skill.file_path.clone(),
                    digest: file_digest(&skill.file_path),
                    lineage: skill.lineage.clone(),
                },
            );
        }

        let mut tracker = Self {
            cwd,
            task_preview: truncate_preview(task_text),
            session_id: None,
            skills_by_path,
            activations: BTreeMap::new(),
            pending_skill_reads: HashMap::new(),
            tool_failures: Vec::new(),
            agent_error: None,
        };
        if let Some(explicit) = explicit_skill {
            tracker.activate_path(
                &explicit.skill_name,
                &explicit.skill_path,
                SkillActivationSource::SlashCommand,
            );
        }
        tracker
    }

    pub fn observe_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart { session_id } => {
                self.session_id = Some(session_id.clone());
            }
            AgentEvent::AgentEnd {
                session_id, error, ..
            } => {
                self.session_id = Some(session_id.clone());
                if let Some(error) = error {
                    self.agent_error = Some(error.clone());
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                if tool_name == "read" {
                    if let Some(path) = resolve_tool_path(&self.cwd, args) {
                        let canonical = canonicalize_with_fallback(&path);
                        if self.skills_by_path.contains_key(&canonical) {
                            if let Some(skill) = self.skills_by_path.get(&canonical).cloned() {
                                self.activations.entry(skill.name.clone()).or_insert(
                                    ActivatedSkill {
                                        skill,
                                        source: SkillActivationSource::ReadTool,
                                    },
                                );
                            }
                            self.pending_skill_reads
                                .insert(tool_call_id.clone(), canonical);
                        }
                    }
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                if let Some(path) = self.pending_skill_reads.remove(tool_call_id) {
                    if !*is_error {
                        if let Some(skill) = self.skills_by_path.get(&path).cloned() {
                            self.activations
                                .entry(skill.name.clone())
                                .or_insert(ActivatedSkill {
                                    skill,
                                    source: SkillActivationSource::ReadTool,
                                });
                        }
                    }
                }

                if *is_error {
                    self.tool_failures
                        .push(extract_tool_failure(tool_name, result));
                }
            }
            _ => {}
        }
    }

    pub fn record_runtime_error(&mut self, error: impl Into<String>) {
        self.agent_error = Some(error.into());
    }

    fn activate_path(&mut self, skill_name: &str, path: &Path, source: SkillActivationSource) {
        let canonical = canonicalize_with_fallback(path);
        let skill = self
            .skills_by_path
            .get(&canonical)
            .cloned()
            .unwrap_or_else(|| TrackedSkill {
                skill_id: legacy_skill_id(path),
                name: skill_name.to_string(),
                path: path.to_path_buf(),
                digest: file_digest(path),
                lineage: SkillLineage::default(),
            });
        self.activations
            .entry(skill.name.clone())
            .or_insert(ActivatedSkill { skill, source });
    }

    pub fn observations(&self) -> Vec<SkillObservation> {
        let outcome = classify_outcome(&self.tool_failures, self.agent_error.as_deref());
        self.activations
            .values()
            .map(|activation| SkillObservation {
                version: 2,
                recorded_at_utc: timestamp_now(),
                session_id: self.session_id.clone(),
                skill_id: activation.skill.skill_id.clone(),
                skill_name: activation.skill.name.clone(),
                skill_path: activation.skill.path.clone(),
                skill_digest: activation.skill.digest.clone(),
                activation_source: activation.source,
                task_preview: self.task_preview.clone(),
                outcome,
                lineage: activation.skill.lineage.clone(),
                tool_failures: self.tool_failures.clone(),
                agent_error: self.agent_error.clone(),
            })
            .collect()
    }
}

pub fn persist_skill_tracker(
    tracker: &SkillRunTracker,
    session: Option<&mut Session>,
) -> Result<Vec<SkillObservation>> {
    let observations = tracker.observations();
    if observations.is_empty() {
        return Ok(Vec::new());
    }

    append_jsonl_records(&skill_observation_ledger_path(&tracker.cwd), &observations)?;

    if let Some(session) = session {
        for observation in &observations {
            let data = serde_json::to_value(observation)
                .map_err(|err| Error::session(format!("serialize skill observation: {err}")))?;
            session.append_custom_entry(SKILL_OBSERVATION_ENTRY_TYPE.to_string(), Some(data));
        }
    }

    Ok(observations)
}

pub fn handle_skill_feedback(
    cwd: &Path,
    skill_name: &str,
    rating: u8,
    notes: Option<&str>,
    session_id: Option<&str>,
    format: SkillDoctorFormat,
) -> Result<SkillFeedback> {
    if !(1..=5).contains(&rating) {
        return Err(Error::validation(format!(
            "Skill feedback rating must be between 1 and 5 inclusive: {rating}"
        )));
    }

    let observations = load_observations(cwd)?;
    let target = resolve_skill_feedback_target(cwd, skill_name, session_id, &observations)?;
    let feedback = SkillFeedback {
        version: 2,
        recorded_at_utc: timestamp_now(),
        session_id: normalize_optional_text(session_id),
        skill_id: target.skill_id,
        skill_name: target.name,
        skill_path: target.path,
        skill_digest: target.digest,
        lineage: target.lineage,
        rating,
        notes: normalize_optional_text(notes).unwrap_or_default(),
    };

    append_jsonl_records(&skill_feedback_ledger_path(cwd), &[feedback.clone()])?;

    match format {
        SkillDoctorFormat::Text => {
            println!("{}", render_skill_feedback_receipt(&feedback)?);
        }
        SkillDoctorFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&feedback)
                    .map_err(|err| Error::session(format!("serialize skill feedback: {err}")))?
            );
        }
    }

    Ok(feedback)
}

pub fn handle_skill_doctor(
    cwd: &Path,
    format: SkillDoctorFormat,
    fix: bool,
) -> Result<SkillDoctorReport> {
    let criteria = SkillSuccessCriteria::default();
    let observations = load_observations(cwd)?;
    let feedback = load_feedback(cwd)?;
    let amendments = load_amendments(cwd)?;
    let mut grouped_observations: BTreeMap<String, Vec<SkillObservation>> = BTreeMap::new();
    for observation in observations.iter().cloned() {
        grouped_observations
            .entry(observation.skill_id.clone())
            .or_default()
            .push(observation);
    }
    let mut grouped_feedback: BTreeMap<String, Vec<SkillFeedback>> = BTreeMap::new();
    for entry in feedback.iter().cloned() {
        grouped_feedback
            .entry(entry.skill_id.clone())
            .or_default()
            .push(entry);
    }

    let mut skill_ids = BTreeSet::new();
    skill_ids.extend(grouped_observations.keys().cloned());
    skill_ids.extend(grouped_feedback.keys().cloned());
    let mut skills = Vec::new();
    for skill_id in skill_ids {
        let mut entries = grouped_observations.remove(&skill_id).unwrap_or_default();
        entries.sort_by(|a, b| a.recorded_at_utc.cmp(&b.recorded_at_utc));
        let mut feedback_entries = grouped_feedback.remove(&skill_id).unwrap_or_default();
        feedback_entries.sort_by(|a, b| a.recorded_at_utc.cmp(&b.recorded_at_utc));
        let skill_name = entries
            .last()
            .map(|entry| entry.skill_name.clone())
            .or_else(|| {
                feedback_entries
                    .last()
                    .map(|entry| entry.skill_name.clone())
            })
            .unwrap_or_else(|| skill_id.clone());
        skills.push(build_skill_health_report(
            &skill_id,
            &skill_name,
            &entries,
            &feedback_entries,
            &criteria,
        ));
    }
    skills.sort_by(|a, b| {
        a.skill_name
            .cmp(&b.skill_name)
            .then_with(|| a.skill_id.cmp(&b.skill_id))
    });

    let mut applied_amendments = Vec::new();
    if fix {
        for index in 0..skills.len() {
            match skills[index].status {
                SkillHealthStatus::Regressed => {
                    if let Some(amendment) = rollback_managed_skill_patch(
                        &skills[index].skill_name,
                        &skills[index].skill_path,
                        &skills[index].current_digest,
                        &skills[index].evidence,
                        &amendments,
                        cwd,
                    )? {
                        mark_report_pending_for_unobserved_revision(
                            &mut skills[index],
                            &criteria,
                            amendment.new_digest.clone(),
                        );
                        applied_amendments.push(amendment);
                    }
                }
                SkillHealthStatus::NeedsAmendment => {
                    if skills[index].proposed_guardrails.is_empty()
                        && skills[index].proposed_routing_hints.is_empty()
                    {
                        continue;
                    }
                    if let Some(amendment) = apply_managed_skill_patch(
                        &skills[index].skill_name,
                        &skills[index].skill_path,
                        &skills[index].proposed_guardrails,
                        &skills[index].proposed_routing_hints,
                        &skills[index].evidence,
                        cwd,
                    )? {
                        mark_report_pending_for_unobserved_revision(
                            &mut skills[index],
                            &criteria,
                            amendment.new_digest.clone(),
                        );
                        applied_amendments.push(amendment);
                    }
                }
                _ => {}
            }
        }
    }

    let loaded_skills = load_skills(LoadSkillsOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: Config::global_dir(),
        skill_paths: Vec::new(),
        include_defaults: true,
    });
    let producers = build_skill_producer_reports(
        &loaded_skills.skills,
        &skills,
        &observations,
        &feedback,
        &amendments,
    );
    let report = SkillDoctorReport {
        criteria,
        observation_count: observations.len(),
        feedback_count: feedback.len(),
        skills,
        producers,
        applied_amendments,
    };

    match format {
        SkillDoctorFormat::Text => {
            println!("{}", render_skill_doctor_report(&report)?);
        }
        SkillDoctorFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|err| Error::session(format!(
                    "serialize skills doctor report: {err}"
                )))?
            );
        }
    }

    Ok(report)
}

fn build_skill_health_report(
    skill_id: &str,
    skill_name: &str,
    observations: &[SkillObservation],
    feedback: &[SkillFeedback],
    criteria: &SkillSuccessCriteria,
) -> SkillHealthReport {
    let (latest_path, latest_digest) = if let Some(latest) = observations.last() {
        (latest.skill_path.clone(), latest.skill_digest.clone())
    } else if let Some(latest) = feedback.last() {
        (latest.skill_path.clone(), latest.skill_digest.clone())
    } else {
        panic!("skill evidence should not be empty");
    };

    let (previous_observations, latest_observations) =
        split_observations_by_digest(observations, &latest_digest);
    let (previous_feedback, latest_feedback) = split_feedback_by_digest(feedback, &latest_digest);
    let current_digest = current_skill_revision_digest(&latest_path, skill_id);
    let run_count = latest_observations.len();
    let success_rate = success_rate(latest_observations);
    let consecutive_failures = consecutive_failures(latest_observations);
    let feedback_count = latest_feedback.len();
    let average_feedback_score = average_feedback_score(latest_feedback);
    let low_feedback_count = low_feedback_count(latest_feedback);
    let evidence = summarize_evidence(latest_observations, latest_feedback);
    let mut proposed_guardrails =
        recommend_guardrails(latest_observations, latest_feedback, criteria);
    let mut proposed_routing_hints =
        recommend_routing_hints(latest_observations, latest_feedback, criteria);

    let has_unobserved_revision = current_digest != "missing" && current_digest != latest_digest;
    if has_unobserved_revision {
        let mut report = SkillHealthReport {
            skill_id: skill_id.to_string(),
            skill_name: skill_name.to_string(),
            skill_path: latest_path,
            latest_digest,
            current_digest,
            run_count,
            success_rate,
            consecutive_failures,
            feedback_count,
            average_feedback_score,
            low_feedback_count,
            status: SkillHealthStatus::PendingData,
            evidence,
            proposed_guardrails,
            proposed_routing_hints,
            previous_success_rate: Some(success_rate),
            latest_success_rate: None,
            previous_average_feedback_score: average_feedback_score,
            latest_average_feedback_score: None,
        };
        let pending_digest = report.current_digest.clone();
        mark_report_pending_for_unobserved_revision(&mut report, criteria, pending_digest);
        return report;
    }

    let unhealthy = run_count >= criteria.min_sample_size
        && (success_rate < criteria.min_success_rate
            || consecutive_failures >= criteria.max_consecutive_failures)
        || feedback_count >= criteria.min_feedback_sample_size
            && average_feedback_score
                .is_some_and(|score| score < criteria.min_average_feedback_score);

    let evaluation = evaluate_latest_revision(
        previous_observations,
        latest_observations,
        previous_feedback,
        latest_feedback,
        criteria,
        unhealthy,
    );
    if matches!(
        evaluation.status,
        SkillHealthStatus::Healthy | SkillHealthStatus::Improved
    ) {
        proposed_guardrails.clear();
        proposed_routing_hints = SkillRoutingHints::default();
    }

    SkillHealthReport {
        skill_id: skill_id.to_string(),
        skill_name: skill_name.to_string(),
        skill_path: latest_path,
        latest_digest,
        current_digest,
        run_count,
        success_rate,
        consecutive_failures,
        feedback_count,
        average_feedback_score,
        low_feedback_count,
        status: evaluation.status,
        evidence,
        proposed_guardrails,
        proposed_routing_hints,
        previous_success_rate: evaluation.previous_success_rate,
        latest_success_rate: evaluation.latest_success_rate,
        previous_average_feedback_score: evaluation.previous_average_feedback_score,
        latest_average_feedback_score: evaluation.latest_average_feedback_score,
    }
}

fn mark_report_pending_for_unobserved_revision(
    report: &mut SkillHealthReport,
    criteria: &SkillSuccessCriteria,
    current_digest: String,
) {
    report.current_digest = current_digest;
    report.status = SkillHealthStatus::PendingData;
    report.proposed_guardrails.clear();
    report.proposed_routing_hints = SkillRoutingHints::default();
    report
        .previous_success_rate
        .get_or_insert(report.success_rate);
    report.latest_success_rate = None;
    if let Some(score) = report.average_feedback_score {
        report.previous_average_feedback_score.get_or_insert(score);
    }
    report.latest_average_feedback_score = None;

    let evidence = pending_revision_evidence(criteria);
    if !report.evidence.iter().any(|entry| entry == &evidence) {
        report.evidence.insert(0, evidence);
    }
}

fn build_skill_producer_reports(
    loaded_skills: &[Skill],
    skill_reports: &[SkillHealthReport],
    observations: &[SkillObservation],
    feedback: &[SkillFeedback],
    amendments: &[SkillAmendment],
) -> Vec<SkillProducerReport> {
    let reports_by_id = skill_reports
        .iter()
        .cloned()
        .map(|report| (report.skill_id.clone(), report))
        .collect::<HashMap<_, _>>();
    let loaded_by_id = loaded_skills
        .iter()
        .map(|skill| (skill.skill_id.clone(), skill))
        .collect::<HashMap<_, _>>();
    let snapshots = build_latest_skill_snapshots(observations, feedback, amendments);

    let mut skill_ids = BTreeSet::new();
    skill_ids.extend(reports_by_id.keys().cloned());
    skill_ids.extend(loaded_by_id.keys().cloned());
    skill_ids.extend(snapshots.keys().cloned());

    let mut grouped: BTreeMap<SkillProducerKey, BTreeMap<String, ProducerDescendant>> =
        BTreeMap::new();
    for skill_id in skill_ids {
        let report = reports_by_id.get(&skill_id).cloned();
        let loaded_skill = loaded_by_id.get(&skill_id).copied();
        let snapshot = snapshots.get(&skill_id);
        let lineage = snapshot
            .map(|snapshot| snapshot.lineage.clone())
            .or_else(|| loaded_skill.map(|skill| skill.lineage.clone()))
            .unwrap_or_default();
        if lineage.is_empty() {
            continue;
        }

        let skill_name = report
            .as_ref()
            .map(|report| report.skill_name.clone())
            .or_else(|| loaded_skill.map(|skill| skill.name.clone()))
            .or_else(|| snapshot.map(|snapshot| snapshot.skill_name.clone()))
            .unwrap_or_else(|| skill_id.clone());
        let orphaned = report
            .as_ref()
            .is_some_and(|report| report.current_digest == "missing")
            || loaded_skill.is_none() && snapshot.is_some();
        let descendant = ProducerDescendant {
            skill_id: skill_id.clone(),
            skill_name,
            report,
            orphaned,
        };

        for key in producer_keys_from_lineage(&lineage) {
            grouped
                .entry(key)
                .or_default()
                .insert(skill_id.clone(), descendant.clone());
        }
    }

    let mut reports = grouped
        .into_iter()
        .map(|(key, descendants)| {
            build_skill_producer_report(key, &descendants.into_values().collect::<Vec<_>>())
        })
        .collect::<Vec<_>>();
    reports.sort_by(|left, right| {
        left.producer_skill_name
            .cmp(&right.producer_skill_name)
            .then_with(|| left.role.cmp(&right.role))
            .then_with(|| left.producer_revision.cmp(&right.producer_revision))
    });
    reports
}

fn build_skill_producer_report(
    key: SkillProducerKey,
    descendants: &[ProducerDescendant],
) -> SkillProducerReport {
    let descendant_skill_count = descendants.len();
    let observed_descendant_skill_count = descendants
        .iter()
        .filter(|descendant| descendant.report.is_some())
        .count();
    let unobserved_descendant_skill_count =
        descendant_skill_count - observed_descendant_skill_count;
    let orphaned_descendant_skill_count = descendants
        .iter()
        .filter(|descendant| descendant.orphaned)
        .count();
    let mut effective_descendant_skill_count = 0usize;
    let mut pending_descendant_skill_count = 0usize;
    let mut needs_amendment_descendant_skill_count = 0usize;
    let mut regressed_descendant_skill_count = 0usize;
    let mut success_rates = Vec::new();
    let mut feedback_scores = Vec::new();
    let mut evidence_counts: BTreeMap<String, usize> = BTreeMap::new();

    for descendant in descendants {
        let Some(report) = descendant.report.as_ref() else {
            continue;
        };
        if report.run_count > 0 {
            success_rates.push(report.success_rate);
        }
        if let Some(score) = report.average_feedback_score {
            feedback_scores.push(score);
        }
        match report.status {
            SkillHealthStatus::Healthy | SkillHealthStatus::Improved => {
                effective_descendant_skill_count += 1;
            }
            SkillHealthStatus::PendingData => {
                pending_descendant_skill_count += 1;
            }
            SkillHealthStatus::NeedsAmendment => {
                needs_amendment_descendant_skill_count += 1;
            }
            SkillHealthStatus::Regressed => {
                regressed_descendant_skill_count += 1;
            }
        }

        if matches!(
            report.status,
            SkillHealthStatus::NeedsAmendment | SkillHealthStatus::Regressed
        ) {
            for item in &report.evidence {
                *evidence_counts
                    .entry(normalize_signature(item))
                    .or_default() += 1;
            }
        }
    }

    let effective_descendant_rate = if descendant_skill_count == 0 {
        0.0
    } else {
        effective_descendant_skill_count as f64 / descendant_skill_count as f64
    };
    let average_descendant_success_rate = (!success_rates.is_empty())
        .then(|| success_rates.iter().sum::<f64>() / success_rates.len() as f64);
    let average_descendant_feedback_score = (!feedback_scores.is_empty())
        .then(|| feedback_scores.iter().sum::<f64>() / feedback_scores.len() as f64);

    let mut evidence = vec![format!(
        "{effective_descendant_skill_count}/{descendant_skill_count} descendant skills are healthy or improved."
    )];
    if unobserved_descendant_skill_count > 0 {
        evidence.push(format!(
            "{unobserved_descendant_skill_count} descendant skill(s) have no runtime evidence yet."
        ));
    }
    if orphaned_descendant_skill_count > 0 {
        let orphaned_names = descendants
            .iter()
            .filter(|descendant| descendant.orphaned)
            .map(|descendant| format!("{} ({})", descendant.skill_name, descendant.skill_id))
            .collect::<Vec<_>>();
        evidence.push(format!(
            "{orphaned_descendant_skill_count} descendant skill(s) are orphaned from the current filesystem state: {}.",
            orphaned_names.join(", ")
        ));
    }
    if pending_descendant_skill_count > 0 {
        evidence.push(format!(
            "{pending_descendant_skill_count} descendant skill(s) are still pending post-amend evaluation."
        ));
    }
    if needs_amendment_descendant_skill_count > 0 {
        evidence.push(format!(
            "{needs_amendment_descendant_skill_count} descendant skill(s) currently need amendment."
        ));
    }
    if regressed_descendant_skill_count > 0 {
        evidence.push(format!(
            "{regressed_descendant_skill_count} descendant skill(s) regressed after amendment."
        ));
    }

    let mut ranked_evidence = evidence_counts.into_iter().collect::<Vec<_>>();
    ranked_evidence.sort_by(|(left_message, left_count), (right_message, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_message.cmp(right_message))
    });
    for (message, count) in ranked_evidence.into_iter().take(3) {
        evidence.push(format!("{message} ({count}x across descendants)"));
    }

    SkillProducerReport {
        producer_skill_name: key.name,
        producer_revision: key.revision,
        role: key.role,
        descendant_skill_count,
        observed_descendant_skill_count,
        unobserved_descendant_skill_count,
        orphaned_descendant_skill_count,
        effective_descendant_skill_count,
        pending_descendant_skill_count,
        needs_amendment_descendant_skill_count,
        regressed_descendant_skill_count,
        effective_descendant_rate,
        average_descendant_success_rate,
        average_descendant_feedback_score,
        evidence,
    }
}

fn build_latest_skill_snapshots(
    observations: &[SkillObservation],
    feedback: &[SkillFeedback],
    amendments: &[SkillAmendment],
) -> HashMap<String, SkillEvidenceSnapshot> {
    let mut snapshots = HashMap::new();
    for observation in observations {
        update_skill_snapshot(
            &mut snapshots,
            &observation.skill_id,
            &observation.recorded_at_utc,
            &observation.skill_name,
            &observation.lineage,
        );
    }
    for entry in feedback {
        update_skill_snapshot(
            &mut snapshots,
            &entry.skill_id,
            &entry.recorded_at_utc,
            &entry.skill_name,
            &entry.lineage,
        );
    }
    for amendment in amendments {
        update_skill_snapshot(
            &mut snapshots,
            &amendment.skill_id,
            &amendment.applied_at_utc,
            &amendment.skill_name,
            &amendment.lineage,
        );
    }
    snapshots
}

fn update_skill_snapshot(
    snapshots: &mut HashMap<String, SkillEvidenceSnapshot>,
    skill_id: &str,
    recorded_at_utc: &str,
    skill_name: &str,
    lineage: &SkillLineage,
) {
    if skill_id.trim().is_empty() || lineage.is_empty() {
        return;
    }

    let replace = snapshots
        .get(skill_id)
        .is_none_or(|snapshot| recorded_at_utc >= snapshot.recorded_at_utc.as_str());
    if replace {
        snapshots.insert(
            skill_id.to_string(),
            SkillEvidenceSnapshot {
                recorded_at_utc: recorded_at_utc.to_string(),
                skill_name: skill_name.to_string(),
                lineage: lineage.clone(),
            },
        );
    }
}

fn producer_keys_from_lineage(lineage: &SkillLineage) -> Vec<SkillProducerKey> {
    let mut keys = Vec::new();
    if let Some(name) = lineage
        .created_by_skill
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        keys.push(SkillProducerKey {
            role: SkillProducerRole::Creator,
            name: name.to_string(),
            revision: lineage
                .created_by_revision
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
        });
    }
    if let Some(name) = lineage
        .last_improved_by_skill
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        keys.push(SkillProducerKey {
            role: SkillProducerRole::Improver,
            name: name.to_string(),
            revision: lineage
                .last_improved_by_revision
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
        });
    }
    keys
}

fn pending_revision_evidence(criteria: &SkillSuccessCriteria) -> String {
    format!(
        "Current skill revision has not been observed yet; collect at least {} post-amend runs before judging it.",
        criteria.min_post_amend_runs
    )
}

fn current_skill_revision_digest(skill_path: &Path, expected_skill_id: &str) -> String {
    let raw = match fs::read_to_string(skill_path) {
        Ok(raw) => raw,
        Err(_) => return "missing".to_string(),
    };
    let current_skill_id = skill_id_from_raw(&raw, skill_path);
    if current_skill_id != expected_skill_id {
        return "missing".to_string();
    }
    sha256_hex_standalone(&raw)
}

fn evaluate_latest_revision(
    previous_observations: &[SkillObservation],
    latest_observations: &[SkillObservation],
    previous_feedback: &[SkillFeedback],
    latest_feedback: &[SkillFeedback],
    criteria: &SkillSuccessCriteria,
    unhealthy: bool,
) -> RevisionEvaluation {
    let previous_success_rate =
        (!previous_observations.is_empty()).then(|| success_rate(previous_observations));
    let latest_success_rate =
        (!latest_observations.is_empty()).then(|| success_rate(latest_observations));
    let previous_average_feedback_score = average_feedback_score(previous_feedback);
    let latest_average_feedback_score = average_feedback_score(latest_feedback);
    let has_previous_revision = !previous_observations.is_empty() || !previous_feedback.is_empty();

    if !has_previous_revision {
        let status = if unhealthy {
            SkillHealthStatus::NeedsAmendment
        } else if latest_observations.len() < criteria.min_sample_size
            && latest_feedback.len() < criteria.min_feedback_sample_size
        {
            SkillHealthStatus::PendingData
        } else {
            SkillHealthStatus::Healthy
        };
        return RevisionEvaluation {
            status,
            previous_success_rate: None,
            latest_success_rate,
            previous_average_feedback_score: None,
            latest_average_feedback_score,
        };
    }

    let has_observation_window = latest_observations.len() >= criteria.min_post_amend_runs;
    let has_feedback_window = latest_feedback.len() >= criteria.min_feedback_sample_size;
    if !has_observation_window && !has_feedback_window {
        let status = if unhealthy {
            SkillHealthStatus::NeedsAmendment
        } else {
            SkillHealthStatus::PendingData
        };
        return RevisionEvaluation {
            status,
            previous_success_rate,
            latest_success_rate,
            previous_average_feedback_score,
            latest_average_feedback_score,
        };
    }

    let success_delta = if has_observation_window {
        previous_success_rate
            .zip(latest_success_rate)
            .map(|(previous, latest)| latest - previous)
    } else {
        None
    };
    let feedback_delta = if has_feedback_window {
        previous_average_feedback_score
            .zip(latest_average_feedback_score)
            .map(|(previous, latest)| latest - previous)
    } else {
        None
    };
    let regressed = success_delta.is_some_and(|delta| delta <= -criteria.min_improvement_delta)
        || feedback_delta.is_some_and(|delta| delta <= -criteria.min_feedback_improvement_delta);
    let improved = success_delta.is_some_and(|delta| delta >= criteria.min_improvement_delta)
        || feedback_delta.is_some_and(|delta| delta >= criteria.min_feedback_improvement_delta);

    let status = if regressed {
        SkillHealthStatus::Regressed
    } else if improved {
        SkillHealthStatus::Improved
    } else if unhealthy {
        SkillHealthStatus::NeedsAmendment
    } else {
        SkillHealthStatus::Healthy
    };

    RevisionEvaluation {
        status,
        previous_success_rate,
        latest_success_rate,
        previous_average_feedback_score,
        latest_average_feedback_score,
    }
}

fn recommend_guardrails(
    observations: &[SkillObservation],
    feedback: &[SkillFeedback],
    criteria: &SkillSuccessCriteria,
) -> Vec<String> {
    let mut guardrails = Vec::new();
    let mut failure_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut agent_failure_count = 0usize;

    for entry in observations {
        if matches!(
            entry.outcome,
            SkillRunOutcome::AgentFailure | SkillRunOutcome::Aborted
        ) && entry.tool_failures.is_empty()
        {
            agent_failure_count += 1;
        }

        for failure in &entry.tool_failures {
            *failure_counts
                .entry(failure.tool_name.as_str())
                .or_default() += 1;
        }
    }

    if failure_counts.get("read").copied().unwrap_or_default() >= 2 {
        guardrails.push(
            "Resolve every relative path against the skill directory before calling `read`, and stop with a clear explanation when the file is missing."
                .to_string(),
        );
    }
    if failure_counts.get("bash").copied().unwrap_or_default() >= 2 {
        guardrails.push(
            "Validate commands, binaries, and working directory assumptions before calling `bash`; if a prerequisite is missing, report it instead of retrying blindly."
                .to_string(),
        );
    }
    let write_failures = failure_counts.get("write").copied().unwrap_or_default()
        + failure_counts.get("edit").copied().unwrap_or_default();
    if write_failures >= 2 {
        guardrails.push(
            "Read the target file first and verify the path exists before invoking `write` or `edit`; do not mutate blind."
                .to_string(),
        );
    }
    if agent_failure_count >= 2 {
        guardrails.push(
            "Activate this skill only when the task explicitly matches its description and all required inputs are present; otherwise explain why the skill does not apply."
                .to_string(),
        );
    }
    let low_feedback = feedback
        .iter()
        .filter(|entry| entry.rating <= LOW_FEEDBACK_THRESHOLD)
        .collect::<Vec<_>>();
    let low_feedback_problem = feedback.len() >= criteria.min_feedback_sample_size
        && average_feedback_score(feedback)
            .is_some_and(|score| score < criteria.min_average_feedback_score);
    if low_feedback_problem {
        let notes = low_feedback
            .iter()
            .map(|entry| entry.notes.trim())
            .filter(|notes| !notes.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();

        if contains_any(
            &notes,
            &[
                "format", "json", "yaml", "table", "bullet", "schema", "field",
            ],
        ) {
            guardrails.push(
                "State the expected output contract before responding, then verify the final answer matches the required format and fields exactly."
                    .to_string(),
            );
        }
        if contains_any(
            &notes,
            &[
                "wrong",
                "incorrect",
                "inaccurate",
                "halluc",
                "made up",
                "false",
            ],
        ) {
            guardrails.push(
                "Add a verification step for every factual or codebase-specific claim; when evidence is missing, say so instead of guessing."
                    .to_string(),
            );
        }
        if contains_any(
            &notes,
            &[
                "missing",
                "incomplete",
                "forgot",
                "skipped",
                "left out",
                "partial",
            ],
        ) {
            guardrails.push(
                "Use a completion checklist before finishing so all requested steps, files, and deliverables are explicitly covered."
                    .to_string(),
            );
        }
        if contains_any(
            &notes,
            &[
                "verbose",
                "too long",
                "wordy",
                "rambling",
                "too much detail",
            ],
        ) {
            guardrails.push(
                "Keep the final response concise by default; include only the requested outcome, verification, and the minimum supporting context."
                    .to_string(),
            );
        }
        if contains_any(
            &notes,
            &[
                "wrong skill",
                "not applicable",
                "irrelevant",
                "bad trigger",
                "misrouted",
                "should not have used",
            ],
        ) {
            guardrails.push(
                "Tighten activation criteria: only use this skill when the task matches its trigger conditions and required inputs are available."
                    .to_string(),
            );
        }
        if low_feedback_problem && guardrails.is_empty() {
            guardrails.push(
                "Add explicit prerequisites, output expectations, and verification steps before acting so future runs can fail early instead of producing low-quality output."
                    .to_string(),
            );
        }
    }
    if (!observations.is_empty() || !feedback.is_empty()) && guardrails.is_empty() {
        guardrails.push(
            "Before each tool call, verify prerequisites and stop with a concrete explanation instead of cascading into secondary failures."
                .to_string(),
        );
    }

    guardrails.sort();
    guardrails.dedup();
    guardrails
}

fn recommend_routing_hints(
    observations: &[SkillObservation],
    feedback: &[SkillFeedback],
    criteria: &SkillSuccessCriteria,
) -> SkillRoutingHints {
    let low_feedback_problem = feedback.len() >= criteria.min_feedback_sample_size
        && average_feedback_score(feedback)
            .is_some_and(|score| score < criteria.min_average_feedback_score);
    let notes = feedback
        .iter()
        .filter(|entry| entry.rating <= LOW_FEEDBACK_THRESHOLD)
        .map(|entry| entry.notes.trim())
        .filter(|notes| !notes.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    let agent_failures = observations
        .iter()
        .filter(|entry| {
            matches!(
                entry.outcome,
                SkillRunOutcome::AgentFailure | SkillRunOutcome::Aborted
            )
        })
        .count();
    let misrouted = contains_any(
        &notes,
        &[
            "wrong skill",
            "not applicable",
            "irrelevant",
            "bad trigger",
            "misrouted",
            "should not have used",
        ],
    ) || agent_failures >= criteria.max_consecutive_failures;

    if !misrouted {
        return SkillRoutingHints::default();
    }

    let mut routing = SkillRoutingHints {
        use_when: vec![
            "Use this skill only when the request directly matches its intended task and all required inputs are present."
                .to_string(),
        ],
        not_for: vec![
            "Do not use this skill for loosely related requests, missing-input situations, or tasks better handled by another skill."
                .to_string(),
        ],
        trigger_examples: collect_task_previews(observations, SkillRunOutcome::Success, 2),
        anti_trigger_examples: collect_non_success_previews(observations, 2),
    };

    if low_feedback_problem && routing.anti_trigger_examples.is_empty() {
        routing.anti_trigger_examples.push(
            "Requests that repeatedly lead to irrelevant or low-quality results for this skill."
                .to_string(),
        );
    }
    dedup_strings(&mut routing.use_when);
    dedup_strings(&mut routing.not_for);
    dedup_strings(&mut routing.trigger_examples);
    dedup_strings(&mut routing.anti_trigger_examples);
    routing
}

fn summarize_evidence(
    observations: &[SkillObservation],
    feedback: &[SkillFeedback],
) -> Vec<String> {
    let mut evidence = Vec::new();
    if let Some(average) = average_feedback_score(feedback) {
        evidence.push(format!(
            "average feedback {:.1}/5 across {} rating(s)",
            average,
            feedback.len()
        ));
    }
    let low_feedback_count = low_feedback_count(feedback);
    if low_feedback_count > 0 {
        evidence.push(format!(
            "{low_feedback_count} low-feedback rating(s) (<= {LOW_FEEDBACK_THRESHOLD}/5)"
        ));
    }

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in observations {
        if let Some(error) = &entry.agent_error {
            *counts
                .entry(format!("agent: {}", normalize_signature(error)))
                .or_default() += 1;
        }
        for failure in &entry.tool_failures {
            *counts
                .entry(format!("{}: {}", failure.tool_name, failure.signature))
                .or_default() += 1;
        }
    }
    for entry in feedback {
        if entry.rating <= LOW_FEEDBACK_THRESHOLD {
            let label = if entry.notes.trim().is_empty() {
                format!("feedback: rating {}/5", entry.rating)
            } else {
                format!("feedback: {}", normalize_signature(&entry.notes))
            };
            *counts.entry(label).or_default() += 1;
        }
    }

    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|(left_message, left_count), (right_message, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_message.cmp(right_message))
    });
    ranked
        .into_iter()
        .take(3)
        .map(|(message, count)| format!("{message} ({count}x)"))
        .for_each(|entry| evidence.push(entry));
    evidence
}

fn apply_managed_skill_patch(
    skill_name: &str,
    skill_path: &Path,
    guardrails: &[String],
    routing_hints: &SkillRoutingHints,
    evidence: &[String],
    cwd: &Path,
) -> Result<Option<SkillAmendment>> {
    let raw = fs::read_to_string(skill_path).map_err(|err| {
        Error::config(format!(
            "failed to read skill file {}: {err}",
            skill_path.display()
        ))
    })?;
    let previous_digest = sha256_hex_standalone(&raw);
    let skill_id = skill_id_from_raw(&raw, skill_path);
    let previous_guardrails = extract_guardrails_from_raw(&raw);
    let previous_routing_hints = extract_routing_hints_from_raw(&raw);
    let block = guardrail_block(guardrails);
    let with_guardrails = replace_or_insert_guardrail_block(&raw, &block);
    let routed = replace_or_insert_routing_block(&with_guardrails, routing_hints);
    let updated =
        stamp_last_improved_lineage(&routed, PI_SKILLS_DOCTOR_NAME, PI_SKILLS_DOCTOR_REVISION)?;
    if updated == raw {
        return Ok(None);
    }

    fs::write(skill_path, &updated).map_err(|err| {
        Error::config(format!(
            "failed to update skill file {}: {err}",
            skill_path.display()
        ))
    })?;
    let new_digest = sha256_hex_standalone(&updated);
    let lineage = parse_skill_lineage(&parse_frontmatter(&updated).frontmatter);
    let amendment = SkillAmendment {
        version: 2,
        applied_at_utc: timestamp_now(),
        skill_id,
        skill_name: skill_name.to_string(),
        skill_path: skill_path.to_path_buf(),
        previous_digest,
        new_digest,
        previous_guardrails,
        guardrails: guardrails.to_vec(),
        previous_routing_hints,
        routing_hints: extract_routing_hints_from_raw(&updated),
        lineage,
        evidence: evidence.to_vec(),
    };
    append_jsonl_records(&skill_amendment_ledger_path(cwd), &[amendment.clone()])?;
    Ok(Some(amendment))
}

fn rollback_managed_skill_patch(
    skill_name: &str,
    skill_path: &Path,
    current_digest: &str,
    evidence: &[String],
    amendments: &[SkillAmendment],
    cwd: &Path,
) -> Result<Option<SkillAmendment>> {
    let Some(source_amendment) = amendments.iter().rev().find(|amendment| {
        amendment.skill_name == skill_name
            && amendment.skill_path == skill_path
            && amendment.new_digest == current_digest
    }) else {
        return Ok(None);
    };

    let raw = fs::read_to_string(skill_path).map_err(|err| {
        Error::config(format!(
            "failed to read skill file {}: {err}",
            skill_path.display()
        ))
    })?;
    let previous_digest = sha256_hex_standalone(&raw);
    let skill_id = skill_id_from_raw(&raw, skill_path);
    let current_guardrails = extract_guardrails_from_raw(&raw);
    let current_routing_hints = extract_routing_hints_from_raw(&raw);
    let restored_guardrails = restore_guardrail_block(&raw, &source_amendment.previous_guardrails);
    let restored = restore_routing_block(
        &restored_guardrails,
        &source_amendment.previous_routing_hints,
    );
    let updated =
        stamp_last_improved_lineage(&restored, PI_SKILLS_DOCTOR_NAME, PI_SKILLS_DOCTOR_REVISION)?;
    if updated == raw {
        return Ok(None);
    }

    fs::write(skill_path, &updated).map_err(|err| {
        Error::config(format!(
            "failed to update skill file {}: {err}",
            skill_path.display()
        ))
    })?;
    let new_digest = sha256_hex_standalone(&updated);
    let mut rollback_evidence = vec![format!(
        "Automatically rolled back regressed managed skill patch from revision {}.",
        shorten_digest(current_digest)
    )];
    rollback_evidence.extend(evidence.iter().cloned());
    let lineage = parse_skill_lineage(&parse_frontmatter(&updated).frontmatter);
    let amendment = SkillAmendment {
        version: 2,
        applied_at_utc: timestamp_now(),
        skill_id,
        skill_name: skill_name.to_string(),
        skill_path: skill_path.to_path_buf(),
        previous_digest,
        new_digest,
        previous_guardrails: current_guardrails,
        guardrails: source_amendment.previous_guardrails.clone(),
        previous_routing_hints: current_routing_hints,
        routing_hints: source_amendment.previous_routing_hints.clone(),
        lineage,
        evidence: rollback_evidence,
    };
    append_jsonl_records(&skill_amendment_ledger_path(cwd), &[amendment.clone()])?;
    Ok(Some(amendment))
}

fn replace_or_insert_guardrail_block(raw: &str, block: &str) -> String {
    if let (Some(start), Some(end)) = (
        raw.find(GUARDRAIL_BLOCK_BEGIN),
        raw.find(GUARDRAIL_BLOCK_END),
    ) {
        let end = end + GUARDRAIL_BLOCK_END.len();
        let mut updated = String::with_capacity(raw.len() + block.len());
        updated.push_str(&raw[..start]);
        updated.push_str(block);
        updated.push_str(&raw[end..]);
        return updated;
    }

    if raw.starts_with("---\n") {
        if let Some(end_frontmatter) = raw[4..].find("\n---\n") {
            let insert_at = 4 + end_frontmatter + 5;
            let mut updated = String::with_capacity(raw.len() + block.len() + 2);
            updated.push_str(&raw[..insert_at]);
            updated.push('\n');
            updated.push_str(block);
            updated.push('\n');
            updated.push_str(&raw[insert_at..]);
            return updated;
        }
    }

    format!("{block}\n\n{raw}")
}

fn restore_guardrail_block(raw: &str, guardrails: &[String]) -> String {
    if guardrails.is_empty() {
        remove_guardrail_block(raw)
    } else {
        replace_or_insert_guardrail_block(raw, &guardrail_block(guardrails))
    }
}

fn remove_guardrail_block(raw: &str) -> String {
    remove_managed_block(raw, GUARDRAIL_BLOCK_BEGIN, GUARDRAIL_BLOCK_END)
}

fn guardrail_block(guardrails: &[String]) -> String {
    let body = guardrails
        .iter()
        .map(|guardrail| format!("- {guardrail}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{GUARDRAIL_BLOCK_BEGIN}\n## Operational Guardrails\n{body}\n{GUARDRAIL_BLOCK_END}")
}

fn extract_guardrails_from_raw(raw: &str) -> Vec<String> {
    let (Some(start), Some(end_marker)) = (
        raw.find(GUARDRAIL_BLOCK_BEGIN),
        raw.find(GUARDRAIL_BLOCK_END),
    ) else {
        return Vec::new();
    };
    raw[start + GUARDRAIL_BLOCK_BEGIN.len()..end_marker]
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("- "))
        .map(ToString::to_string)
        .collect()
}

fn replace_or_insert_routing_block(raw: &str, routing_hints: &SkillRoutingHints) -> String {
    let base = remove_routing_block(raw);
    if routing_hints.is_empty() {
        return base;
    }

    let block = routing_block(&base, routing_hints);
    if block.is_empty() {
        return base;
    }

    let trimmed = base.trim_end_matches('\n');
    if trimmed.is_empty() {
        format!("{block}\n")
    } else {
        format!("{trimmed}\n\n{block}\n")
    }
}

fn restore_routing_block(raw: &str, routing_hints: &SkillRoutingHints) -> String {
    replace_or_insert_routing_block(raw, routing_hints)
}

fn remove_routing_block(raw: &str) -> String {
    remove_managed_block(raw, ROUTING_BLOCK_BEGIN, ROUTING_BLOCK_END)
}

fn routing_block(raw: &str, routing_hints: &SkillRoutingHints) -> String {
    let sections = parse_skill_sections(&strip_frontmatter(raw));
    let effective = SkillRoutingHints {
        use_when: merge_section_items(&sections.use_when, &routing_hints.use_when),
        not_for: merge_section_items(&sections.not_for, &routing_hints.not_for),
        trigger_examples: merge_section_items(
            &sections.trigger_examples,
            &routing_hints.trigger_examples,
        ),
        anti_trigger_examples: merge_section_items(
            &sections.anti_trigger_examples,
            &routing_hints.anti_trigger_examples,
        ),
    };
    if effective.is_empty() {
        return String::new();
    }

    let mut lines = vec![ROUTING_BLOCK_BEGIN.to_string()];
    push_routing_section(&mut lines, "Use When", &effective.use_when);
    push_routing_section(&mut lines, "Not For", &effective.not_for);
    push_routing_section(&mut lines, "Trigger Examples", &effective.trigger_examples);
    push_routing_section(
        &mut lines,
        "Anti-Trigger Examples",
        &effective.anti_trigger_examples,
    );
    lines.push(ROUTING_BLOCK_END.to_string());
    lines.join("\n")
}

fn push_routing_section(lines: &mut Vec<String>, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }

    if lines.len() > 1 {
        lines.push(String::new());
    }
    lines.push(format!("## {heading}"));
    lines.extend(items.iter().map(|item| format!("- {item}")));
}

fn extract_routing_hints_from_raw(raw: &str) -> SkillRoutingHints {
    let (Some(start), Some(end_marker)) =
        (raw.find(ROUTING_BLOCK_BEGIN), raw.find(ROUTING_BLOCK_END))
    else {
        return SkillRoutingHints::default();
    };
    let sections = parse_skill_sections(&raw[start + ROUTING_BLOCK_BEGIN.len()..end_marker].trim());
    SkillRoutingHints::from_sections(&sections)
}

fn stamp_last_improved_lineage(raw: &str, skill_name: &str, revision: &str) -> Result<String> {
    let parsed = parse_frontmatter(raw);
    let mut frontmatter = parsed.frontmatter;
    let metadata = frontmatter
        .remove("metadata")
        .unwrap_or_else(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let mut metadata_map = match metadata {
        serde_yaml::Value::Mapping(mapping) => mapping,
        _ => serde_yaml::Mapping::new(),
    };

    let provenance_key = serde_yaml::Value::String("provenance".to_string());
    let mut provenance_map = metadata_map
        .remove(&provenance_key)
        .and_then(|value| value.as_mapping().cloned())
        .unwrap_or_default();
    provenance_map.insert(
        serde_yaml::Value::String("last-improved-by-skill".to_string()),
        serde_yaml::Value::String(skill_name.to_string()),
    );
    provenance_map.insert(
        serde_yaml::Value::String("last-improved-by-revision".to_string()),
        serde_yaml::Value::String(revision.to_string()),
    );
    metadata_map.insert(provenance_key, serde_yaml::Value::Mapping(provenance_map));
    frontmatter.insert(
        "metadata".to_string(),
        serde_yaml::Value::Mapping(metadata_map),
    );
    render_skill_raw_with_frontmatter(&frontmatter, &parsed.body)
}

fn render_skill_raw_with_frontmatter(
    frontmatter: &HashMap<String, serde_yaml::Value>,
    body: &str,
) -> Result<String> {
    let mut keys = frontmatter.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    let mut mapping = serde_yaml::Mapping::new();
    for key in keys {
        if let Some(value) = frontmatter.get(&key) {
            mapping.insert(serde_yaml::Value::String(key), value.clone());
        }
    }
    let mut yaml = serde_yaml::to_string(&mapping)
        .map_err(|err| Error::session(format!("serialize skill frontmatter: {err}")))?;
    if let Some(stripped) = yaml.strip_prefix("---\n") {
        yaml = stripped.to_string();
    }
    let yaml = yaml.trim_end();
    let body = body.trim_start_matches('\n');
    if yaml.is_empty() {
        return Ok(body.to_string());
    }
    if body.is_empty() {
        Ok(format!("---\n{yaml}\n---\n"))
    } else {
        Ok(format!("---\n{yaml}\n---\n{body}"))
    }
}

fn merge_section_items(base: &[String], managed: &[String]) -> Vec<String> {
    let mut items = base
        .iter()
        .chain(managed.iter())
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    normalize_items(&mut items);
    items
}

fn normalize_items(items: &mut Vec<String>) {
    items.retain(|item| !item.trim().is_empty());
    for item in items.iter_mut() {
        *item = item.trim().to_string();
    }
    dedup_strings(items);
}

fn remove_managed_block(raw: &str, begin_marker: &str, end_marker: &str) -> String {
    let (Some(start), Some(end_marker_start)) = (raw.find(begin_marker), raw.find(end_marker))
    else {
        return raw.to_string();
    };
    let mut remove_start = start;
    while remove_start > 0 && raw[..remove_start].ends_with('\n') {
        remove_start -= 1;
    }
    let mut remove_end = end_marker_start + end_marker.len();
    while remove_end < raw.len() && raw[remove_end..].starts_with('\n') {
        remove_end += 1;
    }

    let mut updated = String::with_capacity(raw.len());
    updated.push_str(&raw[..remove_start]);
    if !updated.is_empty() && !raw[remove_end..].is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&raw[remove_end..]);
    if raw.ends_with('\n') && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated
}

fn render_skill_doctor_report(report: &SkillDoctorReport) -> Result<String> {
    let mut out = String::new();
    writeln!(
        out,
        "Success criteria: min sample {}, target success rate {:.0}%, max consecutive failures {}, min feedback sample {}, target average feedback {:.1}/5, min post-amend runs {}, required success-rate improvement {:.0}%, required feedback improvement {:.1}",
        report.criteria.min_sample_size,
        report.criteria.min_success_rate * 100.0,
        report.criteria.max_consecutive_failures,
        report.criteria.min_feedback_sample_size,
        report.criteria.min_average_feedback_score,
        report.criteria.min_post_amend_runs,
        report.criteria.min_improvement_delta * 100.0,
        report.criteria.min_feedback_improvement_delta,
    )
    .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
    writeln!(out, "Observed runs: {}", report.observation_count)
        .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
    writeln!(out, "Recorded feedback: {}", report.feedback_count)
        .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;

    if report.skills.is_empty() {
        writeln!(out, "No skill observations recorded yet.")
            .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
    } else {
        let average_feedback_display = |score: Option<f64>| {
            score.map_or_else(|| "n/a".to_string(), |score| format!("{score:.1}/5"))
        };
        for skill in &report.skills {
            writeln!(
                out,
                "{}: {:?} | runs={} success={:.0}% consecutive_failures={} feedback={} avg_feedback={}",
                skill.skill_name,
                skill.status,
                skill.run_count,
                skill.success_rate * 100.0,
                skill.consecutive_failures,
                skill.feedback_count,
                average_feedback_display(skill.average_feedback_score),
            )
            .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            writeln!(out, "  skill_id: {}", skill.skill_id)
                .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            writeln!(out, "  path: {}", skill.skill_path.display())
                .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            if skill.current_digest != skill.latest_digest {
                writeln!(
                    out,
                    "  revision: current digest differs from latest observed digest; awaiting fresh observations"
                )
                .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
            if !skill.evidence.is_empty() {
                writeln!(out, "  evidence: {}", skill.evidence.join("; "))
                    .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
            if !skill.proposed_guardrails.is_empty() {
                writeln!(
                    out,
                    "  proposed guardrails: {}",
                    skill.proposed_guardrails.join(" | ")
                )
                .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
            if !skill.proposed_routing_hints.is_empty() {
                writeln!(
                    out,
                    "  proposed routing: {}",
                    render_routing_hints_summary(&skill.proposed_routing_hints)
                )
                .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
            let mut evaluation_parts = Vec::new();
            if let (Some(previous), Some(latest)) =
                (skill.previous_success_rate, skill.latest_success_rate)
            {
                evaluation_parts.push(format!(
                    "success previous={:.0}% latest={:.0}%",
                    previous * 100.0,
                    latest * 100.0
                ));
            }
            if let (Some(previous), Some(latest)) = (
                skill.previous_average_feedback_score,
                skill.latest_average_feedback_score,
            ) {
                evaluation_parts.push(format!(
                    "feedback previous={previous:.1}/5 latest={latest:.1}/5"
                ));
            }
            if !evaluation_parts.is_empty() {
                writeln!(out, "  evaluation: {}", evaluation_parts.join(" | "))
                    .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
        }
    }

    if !report.producers.is_empty() {
        writeln!(out, "Producer effectiveness: {}", report.producers.len())
            .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
        for producer in &report.producers {
            writeln!(
                out,
                "  {} ({:?}): descendants={} effective={:.0}% observed={} orphaned={} pending={} needs_amendment={} regressed={}",
                producer.producer_skill_name,
                producer.role,
                producer.descendant_skill_count,
                producer.effective_descendant_rate * 100.0,
                producer.observed_descendant_skill_count,
                producer.orphaned_descendant_skill_count,
                producer.pending_descendant_skill_count,
                producer.needs_amendment_descendant_skill_count,
                producer.regressed_descendant_skill_count,
            )
            .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            if let Some(revision) = &producer.producer_revision {
                writeln!(out, "    revision: {revision}")
                    .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
            if let Some(rate) = producer.average_descendant_success_rate {
                writeln!(out, "    avg descendant success: {:.0}%", rate * 100.0)
                    .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
            if let Some(score) = producer.average_descendant_feedback_score {
                writeln!(out, "    avg descendant feedback: {score:.1}/5")
                    .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
            if !producer.evidence.is_empty() {
                writeln!(out, "    evidence: {}", producer.evidence.join("; "))
                    .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            }
        }
    }

    if !report.applied_amendments.is_empty() {
        writeln!(
            out,
            "Applied amendments: {}",
            report.applied_amendments.len()
        )
        .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
        for amendment in &report.applied_amendments {
            writeln!(
                out,
                "  {} -> {}",
                amendment.skill_name,
                amendment.skill_path.display()
            )
            .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
        }
    }

    Ok(out)
}

fn render_routing_hints_summary(hints: &SkillRoutingHints) -> String {
    let mut parts = Vec::new();
    if !hints.use_when.is_empty() {
        parts.push(format!("use_when={}", hints.use_when.join(" | ")));
    }
    if !hints.not_for.is_empty() {
        parts.push(format!("not_for={}", hints.not_for.join(" | ")));
    }
    if !hints.trigger_examples.is_empty() {
        parts.push(format!(
            "trigger_examples={}",
            hints.trigger_examples.join(" | ")
        ));
    }
    if !hints.anti_trigger_examples.is_empty() {
        parts.push(format!(
            "anti_trigger_examples={}",
            hints.anti_trigger_examples.join(" | ")
        ));
    }
    parts.join(" ; ")
}

fn render_skill_feedback_receipt(feedback: &SkillFeedback) -> Result<String> {
    let mut out = String::new();
    writeln!(
        out,
        "Recorded feedback for {}: rating {}/5",
        feedback.skill_name, feedback.rating
    )
    .map_err(|err| Error::session(format!("render skill feedback receipt: {err}")))?;
    writeln!(out, "  path: {}", feedback.skill_path.display())
        .map_err(|err| Error::session(format!("render skill feedback receipt: {err}")))?;
    writeln!(out, "  digest: {}", shorten_digest(&feedback.skill_digest))
        .map_err(|err| Error::session(format!("render skill feedback receipt: {err}")))?;
    if let Some(session_id) = &feedback.session_id {
        writeln!(out, "  session: {session_id}")
            .map_err(|err| Error::session(format!("render skill feedback receipt: {err}")))?;
    }
    if !feedback.notes.is_empty() {
        writeln!(out, "  notes: {}", feedback.notes)
            .map_err(|err| Error::session(format!("render skill feedback receipt: {err}")))?;
    }
    Ok(out)
}

fn append_jsonl_records<T: Serialize>(path: &Path, records: &[T]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            Error::config(format!(
                "failed to create skill ledger directory {}: {err}",
                parent.display()
            ))
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| Error::config(format!("failed to open {}: {err}", path.display())))?;
    for record in records {
        serde_json::to_writer(&mut file, record).map_err(|err| {
            Error::session(format!("failed to serialize {}: {err}", path.display()))
        })?;
        file.write_all(b"\n")
            .map_err(|err| Error::config(format!("failed to write {}: {err}", path.display())))?;
    }
    Ok(())
}

fn load_observations(cwd: &Path) -> Result<Vec<SkillObservation>> {
    let mut entries: Vec<SkillObservation> =
        load_jsonl_records(&skill_observation_ledger_path(cwd))?;
    for entry in &mut entries {
        entry.skill_id = normalize_record_skill_id(&entry.skill_id, &entry.skill_path);
    }
    Ok(entries)
}

fn load_feedback(cwd: &Path) -> Result<Vec<SkillFeedback>> {
    let mut entries: Vec<SkillFeedback> = load_jsonl_records(&skill_feedback_ledger_path(cwd))?;
    for entry in &mut entries {
        entry.skill_id = normalize_record_skill_id(&entry.skill_id, &entry.skill_path);
    }
    Ok(entries)
}

fn load_amendments(cwd: &Path) -> Result<Vec<SkillAmendment>> {
    let mut entries: Vec<SkillAmendment> = load_jsonl_records(&skill_amendment_ledger_path(cwd))?;
    for entry in &mut entries {
        entry.skill_id = normalize_record_skill_id(&entry.skill_id, &entry.skill_path);
    }
    Ok(entries)
}

fn skill_observation_ledger_path(cwd: &Path) -> PathBuf {
    cwd.join(Config::project_dir())
        .join(SKILL_OBSERVATION_LEDGER)
}

fn skill_amendment_ledger_path(cwd: &Path) -> PathBuf {
    cwd.join(Config::project_dir()).join(SKILL_AMENDMENT_LEDGER)
}

fn skill_feedback_ledger_path(cwd: &Path) -> PathBuf {
    cwd.join(Config::project_dir()).join(SKILL_FEEDBACK_LEDGER)
}

fn load_jsonl_records<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(path)
        .map_err(|err| Error::config(format!("failed to open {}: {err}", path.display())))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line =
            line.map_err(|err| Error::config(format!("failed to read {}: {err}", path.display())))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record = serde_json::from_str::<T>(trimmed).map_err(|err| {
            Error::session(format!(
                "failed to parse {} line {}: {err}",
                path.display(),
                index + 1
            ))
        })?;
        records.push(record);
    }
    Ok(records)
}

fn success_rate(entries: &[SkillObservation]) -> f64 {
    if entries.is_empty() {
        return 0.0;
    }
    let successes = entries
        .iter()
        .filter(|entry| entry.outcome.is_success())
        .count();
    successes as f64 / entries.len() as f64
}

fn average_feedback_score(entries: &[SkillFeedback]) -> Option<f64> {
    (!entries.is_empty()).then(|| {
        entries
            .iter()
            .map(|entry| f64::from(entry.rating))
            .sum::<f64>()
            / entries.len() as f64
    })
}

fn low_feedback_count(entries: &[SkillFeedback]) -> usize {
    entries
        .iter()
        .filter(|entry| entry.rating <= LOW_FEEDBACK_THRESHOLD)
        .count()
}

fn collect_task_previews(
    observations: &[SkillObservation],
    outcome: SkillRunOutcome,
    limit: usize,
) -> Vec<String> {
    let mut previews = observations
        .iter()
        .filter(|entry| entry.outcome == outcome)
        .map(|entry| entry.task_preview.trim())
        .filter(|preview| !preview.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    dedup_strings(&mut previews);
    previews.truncate(limit);
    previews
}

fn collect_non_success_previews(observations: &[SkillObservation], limit: usize) -> Vec<String> {
    let mut previews = observations
        .iter()
        .filter(|entry| !entry.outcome.is_success())
        .map(|entry| entry.task_preview.trim())
        .filter(|preview| !preview.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    dedup_strings(&mut previews);
    previews.truncate(limit);
    previews
}

fn dedup_strings(items: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

fn consecutive_failures(entries: &[SkillObservation]) -> usize {
    entries
        .iter()
        .rev()
        .take_while(|entry| !entry.outcome.is_success())
        .count()
}

fn classify_outcome(
    tool_failures: &[SkillToolFailure],
    agent_error: Option<&str>,
) -> SkillRunOutcome {
    if matches!(agent_error, Some("Aborted")) {
        return SkillRunOutcome::Aborted;
    }
    if !tool_failures.is_empty() {
        return SkillRunOutcome::ToolFailure;
    }
    if agent_error.is_some() {
        return SkillRunOutcome::AgentFailure;
    }
    SkillRunOutcome::Success
}

fn extract_tool_failure(tool_name: &str, output: &ToolOutput) -> SkillToolFailure {
    let message = tool_output_summary(output);
    SkillToolFailure {
        tool_name: tool_name.to_string(),
        signature: normalize_signature(&message),
        message,
    }
}

fn tool_output_summary(output: &ToolOutput) -> String {
    let content = output
        .content
        .iter()
        .filter_map(|block| match block {
            crate::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !content.trim().is_empty() {
        return truncate_preview(&content);
    }
    output
        .details
        .as_ref()
        .map(Value::to_string)
        .map(|s| truncate_preview(&s))
        .unwrap_or_else(|| "tool returned an unspecified error".to_string())
}

fn resolve_tool_path(cwd: &Path, args: &Value) -> Option<PathBuf> {
    let path = ["file_path", "path"]
        .into_iter()
        .find_map(|key| args.get(key).and_then(Value::as_str))?;
    let path = PathBuf::from(path);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn canonicalize_with_fallback(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn file_digest(path: &Path) -> String {
    fs::read_to_string(path)
        .map(|raw| sha256_hex_standalone(&raw))
        .unwrap_or_else(|_| "missing".to_string())
}

fn normalize_signature(message: &str) -> String {
    let compact = message
        .lines()
        .next()
        .unwrap_or(message)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_preview(&compact)
}

fn normalize_record_skill_id(skill_id: &str, path: &Path) -> String {
    let trimmed = skill_id.trim();
    if trimmed.is_empty() {
        legacy_skill_id(path)
    } else {
        trimmed.to_string()
    }
}

fn skill_id_from_raw(raw: &str, path: &Path) -> String {
    let parsed = parse_frontmatter(raw);
    parsed
        .frontmatter
        .get("skill-id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| legacy_skill_id(path))
}

fn resolve_skill_feedback_target(
    cwd: &Path,
    skill_name: &str,
    session_id: Option<&str>,
    observations: &[SkillObservation],
) -> Result<SkillFeedbackTarget> {
    if let Some(session_id) = normalize_optional_text(session_id) {
        if let Some(observation) = observations.iter().rev().find(|entry| {
            entry.skill_name == skill_name
                && entry.session_id.as_deref() == Some(session_id.as_str())
        }) {
            return Ok(SkillFeedbackTarget {
                skill_id: normalize_record_skill_id(&observation.skill_id, &observation.skill_path),
                name: observation.skill_name.clone(),
                path: observation.skill_path.clone(),
                digest: observation.skill_digest.clone(),
                lineage: observation.lineage.clone(),
            });
        }
    }

    let loaded = load_skills(LoadSkillsOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: Config::global_dir(),
        skill_paths: Vec::new(),
        include_defaults: true,
    });
    if let Some(skill) = loaded
        .skills
        .into_iter()
        .find(|skill| skill.name == skill_name)
    {
        return Ok(SkillFeedbackTarget {
            skill_id: skill.skill_id,
            name: skill.name,
            digest: file_digest(&skill.file_path),
            path: skill.file_path,
            lineage: skill.lineage,
        });
    }

    if let Some(observation) = observations
        .iter()
        .rev()
        .find(|entry| entry.skill_name == skill_name)
    {
        return Ok(SkillFeedbackTarget {
            skill_id: normalize_record_skill_id(&observation.skill_id, &observation.skill_path),
            name: observation.skill_name.clone(),
            path: observation.skill_path.clone(),
            digest: observation.skill_digest.clone(),
            lineage: observation.lineage.clone(),
        });
    }

    Err(Error::validation(format!(
        "Unknown skill `{skill_name}`; no loaded skill or recorded observation matched."
    )))
}

fn split_observations_by_digest<'a>(
    entries: &'a [SkillObservation],
    latest_digest: &str,
) -> (&'a [SkillObservation], &'a [SkillObservation]) {
    let split_at = entries
        .iter()
        .rposition(|entry| entry.skill_digest != latest_digest)
        .map_or(0, |idx| idx + 1);
    (&entries[..split_at], &entries[split_at..])
}

fn split_feedback_by_digest<'a>(
    entries: &'a [SkillFeedback],
    latest_digest: &str,
) -> (&'a [SkillFeedback], &'a [SkillFeedback]) {
    let split_at = entries
        .iter()
        .rposition(|entry| entry.skill_digest != latest_digest)
        .map_or(0, |idx| idx + 1);
    (&entries[..split_at], &entries[split_at..])
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn normalize_optional_text(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn shorten_digest(digest: &str) -> String {
    const LIMIT: usize = 12;
    if digest.len() <= LIMIT {
        digest.to_string()
    } else {
        digest[..LIMIT].to_string()
    }
}

fn truncate_preview(text: &str) -> String {
    const LIMIT: usize = 180;
    let trimmed = text.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    trimmed.chars().take(LIMIT).collect::<String>() + "..."
}

fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::model::{ContentBlock, TextContent};
    use crate::skills::{SkillLineage, parse_skill_lineage};
    use serde_json::json;
    use tempfile::tempdir;

    fn sample_tool_error(message: &str) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(message.to_string()))],
            details: None,
            is_error: true,
        }
    }

    fn sample_feedback(
        recorded_at_utc: &str,
        session_id: Option<&str>,
        skill_name: &str,
        skill_path: &Path,
        skill_digest: &str,
        rating: u8,
        notes: &str,
    ) -> SkillFeedback {
        SkillFeedback {
            version: 2,
            recorded_at_utc: recorded_at_utc.to_string(),
            session_id: session_id.map(ToString::to_string),
            skill_id: test_skill_id(skill_name),
            skill_name: skill_name.to_string(),
            skill_path: skill_path.to_path_buf(),
            skill_digest: skill_digest.to_string(),
            lineage: SkillLineage::default(),
            rating,
            notes: notes.to_string(),
        }
    }

    fn test_skill_id(skill_name: &str) -> String {
        format!("{skill_name}-id")
    }

    #[test]
    fn tracker_records_explicit_invocation_and_tool_failure() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir.path().join("bug-triage").join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: bug-triage\ndescription: triage bugs\n---\nUse bash\n",
        )
        .expect("write skill");
        let skills = vec![Skill {
            skill_id: test_skill_id("bug-triage"),
            name: "bug-triage".to_string(),
            description: "triage bugs".to_string(),
            file_path: skill_path.clone(),
            base_dir: skill_path.parent().expect("parent").to_path_buf(),
            source: "project".to_string(),
            disable_model_invocation: false,
            sections: SkillSections::default(),
            lineage: SkillLineage::default(),
        }];
        let mut tracker = SkillRunTracker::new(
            dir.path().to_path_buf(),
            "/skill:bug-triage fix the failing test",
            &skills,
            Some(ExplicitSkillInvocation {
                skill_name: "bug-triage".to_string(),
                skill_path: skill_path.clone(),
                args: "fix the failing test".to_string(),
            }),
        );
        tracker.observe_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            result: sample_tool_error("cargo: command not found"),
            is_error: true,
        });
        let observations = tracker.observations();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].skill_name, "bug-triage");
        assert_eq!(observations[0].outcome, SkillRunOutcome::ToolFailure);
    }

    #[test]
    fn doctor_recommends_bash_guardrail_for_repeated_failures() {
        let entries = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix again".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix again".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
        ];
        let report = build_skill_health_report(
            &test_skill_id("bug-triage"),
            "bug-triage",
            &entries,
            &[],
            &SkillSuccessCriteria::default(),
        );
        assert_eq!(report.status, SkillHealthStatus::NeedsAmendment);
        assert!(
            report
                .proposed_guardrails
                .iter()
                .any(|guardrail| guardrail.contains("Validate commands"))
        );
    }

    #[test]
    fn doctor_marks_low_feedback_skill_unhealthy() {
        let feedback = vec![
            sample_feedback(
                "2026-03-15T10:00:00Z",
                None,
                "summarize",
                Path::new("/tmp/SKILL.md"),
                "abc",
                1,
                "wrong json format and missing fields",
            ),
            sample_feedback(
                "2026-03-15T10:05:00Z",
                None,
                "summarize",
                Path::new("/tmp/SKILL.md"),
                "abc",
                2,
                "incomplete output and too verbose",
            ),
        ];
        let report = build_skill_health_report(
            &test_skill_id("summarize"),
            "summarize",
            &[],
            &feedback,
            &SkillSuccessCriteria::default(),
        );
        assert_eq!(report.status, SkillHealthStatus::NeedsAmendment);
        assert_eq!(report.feedback_count, 2);
        assert_eq!(report.average_feedback_score, Some(1.5));
        assert!(
            report
                .proposed_guardrails
                .iter()
                .any(|guardrail| guardrail.contains("output contract"))
        );
        assert!(
            report
                .proposed_guardrails
                .iter()
                .any(|guardrail| guardrail.contains("completion checklist"))
        );
    }

    #[test]
    fn doctor_recommends_routing_hints_for_misrouted_skill() {
        let observations = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "review this rust diff for bugs".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "draft a launch email".to_string(),
                outcome: SkillRunOutcome::AgentFailure,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: Some("Wrong task".to_string()),
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "write release marketing copy".to_string(),
                outcome: SkillRunOutcome::AgentFailure,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: Some("Wrong task".to_string()),
            },
        ];
        let feedback = vec![
            sample_feedback(
                "2026-03-15T10:12:00Z",
                None,
                "code-review",
                Path::new("/tmp/SKILL.md"),
                "abc",
                1,
                "wrong skill, misrouted request",
            ),
            sample_feedback(
                "2026-03-15T10:15:00Z",
                None,
                "code-review",
                Path::new("/tmp/SKILL.md"),
                "abc",
                2,
                "bad trigger for writing tasks",
            ),
        ];

        let report = build_skill_health_report(
            &test_skill_id("code-review"),
            "code-review",
            &observations,
            &feedback,
            &SkillSuccessCriteria::default(),
        );

        assert_eq!(report.status, SkillHealthStatus::NeedsAmendment);
        assert!(
            report
                .proposed_routing_hints
                .use_when
                .iter()
                .any(|item| item.contains("directly matches"))
        );
        assert_eq!(
            report.proposed_routing_hints.trigger_examples,
            vec!["review this rust diff for bugs".to_string()]
        );
        assert_eq!(
            report.proposed_routing_hints.anti_trigger_examples,
            vec![
                "draft a launch email".to_string(),
                "write release marketing copy".to_string(),
            ]
        );
    }

    #[test]
    fn guardrail_patch_replaces_managed_block() {
        let raw = "---\nname: bug-triage\ndescription: triage bugs\n---\nBody\n";
        let block = guardrail_block(&["Guardrail one".to_string(), "Guardrail two".to_string()]);
        let updated = replace_or_insert_guardrail_block(raw, &block);
        let replaced = replace_or_insert_guardrail_block(
            &updated,
            &guardrail_block(&["Guardrail three".to_string()]),
        );
        assert!(replaced.contains("Guardrail three"));
        assert!(!replaced.contains("Guardrail one"));
    }

    #[test]
    fn restore_guardrail_block_removes_managed_block() {
        let raw = "---\nname: bug-triage\ndescription: triage bugs\n---\nBody\n";
        let updated = replace_or_insert_guardrail_block(
            raw,
            &guardrail_block(&["Guardrail one".to_string()]),
        );
        let restored = restore_guardrail_block(&updated, &[]);
        assert_eq!(restored, raw);
    }

    #[test]
    fn routing_patch_overlays_existing_sections() {
        let raw = "---\nname: code-review\ndescription: review code\n---\n## Use When\n- inspect a code diff\n\n## Not For\n- writing product copy\n";
        let updated = replace_or_insert_routing_block(
            raw,
            &SkillRoutingHints {
                use_when: vec!["only when the request is about finding bugs".to_string()],
                not_for: vec!["general writing tasks".to_string()],
                trigger_examples: vec!["review this rust patch for bugs".to_string()],
                anti_trigger_examples: vec!["draft a launch email".to_string()],
            },
        );

        assert!(updated.contains(ROUTING_BLOCK_BEGIN));
        let sections = parse_skill_sections(&strip_frontmatter(&updated));
        assert_eq!(
            sections.use_when,
            vec![
                "inspect a code diff".to_string(),
                "only when the request is about finding bugs".to_string(),
            ]
        );
        assert_eq!(
            sections.not_for,
            vec![
                "writing product copy".to_string(),
                "general writing tasks".to_string(),
            ]
        );
        assert_eq!(
            sections.trigger_examples,
            vec!["review this rust patch for bugs".to_string()]
        );
        assert_eq!(
            sections.anti_trigger_examples,
            vec!["draft a launch email".to_string()]
        );

        let restored = restore_routing_block(&updated, &SkillRoutingHints::default());
        assert_eq!(restored, raw);
    }

    #[test]
    fn tracker_activates_skill_on_successful_read_tool() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir.path().join("summarize").join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: summarize\ndescription: summarize text\n---\nDo it\n",
        )
        .expect("write skill");
        let skills = vec![Skill {
            skill_id: test_skill_id("summarize"),
            name: "summarize".to_string(),
            description: "summarize text".to_string(),
            file_path: skill_path.clone(),
            base_dir: skill_path.parent().expect("parent").to_path_buf(),
            source: "project".to_string(),
            disable_model_invocation: false,
            sections: SkillSections::default(),
            lineage: SkillLineage::default(),
        }];
        let mut tracker =
            SkillRunTracker::new(dir.path().to_path_buf(), "summarize this", &skills, None);
        tracker.observe_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "read-1".to_string(),
            tool_name: "read".to_string(),
            args: json!({ "file_path": skill_path.display().to_string() }),
        });
        tracker.observe_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "read-1".to_string(),
            tool_name: "read".to_string(),
            result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("ok".to_string()))],
                details: None,
                is_error: false,
            },
            is_error: false,
        });
        let observations = tracker.observations();
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].activation_source,
            SkillActivationSource::ReadTool
        );
    }

    #[test]
    fn tracker_records_failed_skill_read_as_observation() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir.path().join("summarize").join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: summarize\ndescription: summarize text\n---\nDo it\n",
        )
        .expect("write skill");
        let skills = vec![Skill {
            skill_id: test_skill_id("summarize"),
            name: "summarize".to_string(),
            description: "summarize text".to_string(),
            file_path: skill_path.clone(),
            base_dir: skill_path.parent().expect("parent").to_path_buf(),
            source: "project".to_string(),
            disable_model_invocation: false,
            sections: SkillSections::default(),
            lineage: SkillLineage::default(),
        }];
        let mut tracker =
            SkillRunTracker::new(dir.path().to_path_buf(), "summarize this", &skills, None);
        tracker.observe_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "read-1".to_string(),
            tool_name: "read".to_string(),
            args: json!({ "file_path": skill_path.display().to_string() }),
        });
        tracker.observe_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "read-1".to_string(),
            tool_name: "read".to_string(),
            result: sample_tool_error("No such file or directory"),
            is_error: true,
        });
        let observations = tracker.observations();
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].activation_source,
            SkillActivationSource::ReadTool
        );
        assert_eq!(observations[0].outcome, SkillRunOutcome::ToolFailure);
        assert!(
            observations[0]
                .tool_failures
                .iter()
                .any(|failure| failure.tool_name == "read")
        );
    }

    #[test]
    fn handle_skill_feedback_prefers_session_observation_revision() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("summarize")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: summarize\nskill-id: summarize-id\ndescription: summarize text\n---\nVersion one\n",
        )
        .expect("write skill");
        let observed_digest = file_digest(&skill_path);
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &[SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                session_id: Some("sess-1".to_string()),
                skill_id: test_skill_id("summarize"),
                skill_name: "summarize".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: observed_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "summarize this".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            }],
        )
        .expect("write observation");

        fs::write(
            &skill_path,
            "---\nname: summarize\nskill-id: summarize-id\ndescription: summarize text\n---\nVersion two\n",
        )
        .expect("rewrite skill");
        let current_digest = file_digest(&skill_path);
        assert_ne!(current_digest, observed_digest);

        let feedback = handle_skill_feedback(
            dir.path(),
            "summarize",
            1,
            Some("wrong output"),
            Some("sess-1"),
            SkillDoctorFormat::Json,
        )
        .expect("record feedback");
        assert_eq!(feedback.skill_digest, observed_digest);

        let stored_feedback = load_feedback(dir.path()).expect("load feedback");
        assert_eq!(stored_feedback.len(), 1);
        assert_eq!(stored_feedback[0].skill_digest, observed_digest);
    }

    #[test]
    fn doctor_fix_then_pending_then_improved() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("bug-triage")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: bug-triage\nskill-id: bug-triage-id\ndescription: triage bugs\n---\nUse bash\n",
        )
        .expect("write skill");
        let original_digest = file_digest(&skill_path);

        let failing_entries = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix again".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix once more".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
        ];
        append_jsonl_records(&skill_observation_ledger_path(dir.path()), &failing_entries)
            .expect("write failing observations");

        let fixed_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, true).expect("doctor fix");
        assert_eq!(fixed_report.applied_amendments.len(), 1);
        let fixed_skill = fixed_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "bug-triage")
            .expect("fixed skill report");
        assert_eq!(fixed_skill.status, SkillHealthStatus::PendingData);
        assert_ne!(fixed_skill.current_digest, fixed_skill.latest_digest);
        let amended_skill = fs::read_to_string(&skill_path).expect("read amended skill");
        assert!(amended_skill.contains(GUARDRAIL_BLOCK_BEGIN));

        let pending_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let pending_skill = pending_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "bug-triage")
            .expect("pending skill report");
        assert_eq!(pending_skill.status, SkillHealthStatus::PendingData);
        assert_ne!(pending_skill.current_digest, pending_skill.latest_digest);
        assert!(pending_skill.proposed_guardrails.is_empty());

        let amended_digest = file_digest(&skill_path);
        let successful_entries = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:20:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix after amendment".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:25:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix after amendment again".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:30:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("bug-triage"),
                skill_name: "bug-triage".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest,
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix after amendment one more time".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
        ];
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &successful_entries,
        )
        .expect("write successful observations");

        let improved_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let improved_skill = improved_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "bug-triage")
            .expect("improved skill report");
        assert_eq!(improved_skill.status, SkillHealthStatus::Improved);
        assert_eq!(improved_skill.previous_success_rate, Some(0.0));
        assert_eq!(improved_skill.latest_success_rate, Some(1.0));
        assert!(improved_skill.proposed_guardrails.is_empty());
    }

    #[test]
    fn doctor_fix_applies_routing_overlay_to_skill_file() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("code-review")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: code-review\nskill-id: code-review-id\ndescription: review code\n---\n## Use When\n- inspect a code diff\n\n## Not For\n- writing product copy\n\n## Instructions\n1. Review the diff\n",
        )
        .expect("write skill");
        let original_digest = file_digest(&skill_path);

        let observations = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "review this rust diff for bugs".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "draft a launch email".to_string(),
                outcome: SkillRunOutcome::AgentFailure,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: Some("Wrong task".to_string()),
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "write release marketing copy".to_string(),
                outcome: SkillRunOutcome::AgentFailure,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: Some("Wrong task".to_string()),
            },
        ];
        append_jsonl_records(&skill_observation_ledger_path(dir.path()), &observations)
            .expect("write observations");
        let feedback = vec![
            sample_feedback(
                "2026-03-15T10:12:00Z",
                None,
                "code-review",
                &skill_path,
                &original_digest,
                1,
                "wrong skill, misrouted request",
            ),
            sample_feedback(
                "2026-03-15T10:15:00Z",
                None,
                "code-review",
                &skill_path,
                &original_digest,
                2,
                "bad trigger for writing tasks",
            ),
        ];
        append_jsonl_records(&skill_feedback_ledger_path(dir.path()), &feedback)
            .expect("write feedback");

        let initial_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let initial_skill = initial_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "code-review")
            .expect("skill report");
        assert_eq!(initial_skill.status, SkillHealthStatus::NeedsAmendment);
        assert!(!initial_skill.proposed_routing_hints.is_empty());

        let fixed_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, true).expect("doctor fix");
        let fixed_skill = fixed_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "code-review")
            .expect("fixed skill report");
        assert_eq!(fixed_skill.status, SkillHealthStatus::PendingData);
        assert_eq!(fixed_report.applied_amendments.len(), 1);

        let amended_raw = fs::read_to_string(&skill_path).expect("read amended skill");
        assert!(amended_raw.contains(ROUTING_BLOCK_BEGIN));
        let sections = parse_skill_sections(&strip_frontmatter(&amended_raw));
        assert_eq!(
            sections.use_when,
            vec![
                "inspect a code diff".to_string(),
                "Use this skill only when the request directly matches its intended task and all required inputs are present."
                    .to_string(),
            ]
        );
        assert_eq!(
            sections.not_for,
            vec![
                "writing product copy".to_string(),
                "Do not use this skill for loosely related requests, missing-input situations, or tasks better handled by another skill."
                    .to_string(),
            ]
        );
        assert_eq!(
            sections.trigger_examples,
            vec!["review this rust diff for bugs".to_string()]
        );
        assert_eq!(
            sections.anti_trigger_examples,
            vec![
                "draft a launch email".to_string(),
                "write release marketing copy".to_string(),
            ]
        );
    }

    #[test]
    fn doctor_uses_feedback_to_improve_successful_skill() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("code-review")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: code-review\nskill-id: code-review-id\ndescription: review code\n---\nReturn findings\n",
        )
        .expect("write skill");
        let original_digest = file_digest(&skill_path);

        let successful_entries = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "review this diff".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "review this diff again".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "review this diff one more time".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
        ];
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &successful_entries,
        )
        .expect("write successful observations");
        let negative_feedback = vec![
            sample_feedback(
                "2026-03-15T10:12:00Z",
                None,
                "code-review",
                &skill_path,
                &original_digest,
                1,
                "wrong format and missing the key bug",
            ),
            sample_feedback(
                "2026-03-15T10:15:00Z",
                None,
                "code-review",
                &skill_path,
                &original_digest,
                2,
                "too verbose and incomplete",
            ),
        ];
        append_jsonl_records(&skill_feedback_ledger_path(dir.path()), &negative_feedback)
            .expect("write negative feedback");

        let initial_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let initial_skill = initial_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "code-review")
            .expect("initial skill report");
        assert_eq!(initial_skill.status, SkillHealthStatus::NeedsAmendment);
        assert_eq!(initial_skill.success_rate, 1.0);
        assert_eq!(initial_skill.average_feedback_score, Some(1.5));

        let fixed_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, true).expect("doctor fix");
        let fixed_skill = fixed_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "code-review")
            .expect("fixed skill report");
        assert_eq!(fixed_skill.status, SkillHealthStatus::PendingData);

        let amended_digest = file_digest(&skill_path);
        let amended_observations = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:20:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "review after amendment".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:25:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "review after amendment again".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:30:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("code-review"),
                skill_name: "code-review".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "review after amendment one more time".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
        ];
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &amended_observations,
        )
        .expect("write amended observations");
        let positive_feedback = vec![
            sample_feedback(
                "2026-03-15T10:32:00Z",
                None,
                "code-review",
                &skill_path,
                &amended_digest,
                5,
                "found the key bug and kept the format tight",
            ),
            sample_feedback(
                "2026-03-15T10:35:00Z",
                None,
                "code-review",
                &skill_path,
                &amended_digest,
                4,
                "clear and complete",
            ),
        ];
        append_jsonl_records(&skill_feedback_ledger_path(dir.path()), &positive_feedback)
            .expect("write positive feedback");

        let improved_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let improved_skill = improved_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "code-review")
            .expect("improved skill report");
        assert_eq!(improved_skill.status, SkillHealthStatus::Improved);
        assert_eq!(improved_skill.previous_success_rate, Some(1.0));
        assert_eq!(improved_skill.latest_success_rate, Some(1.0));
        assert_eq!(improved_skill.previous_average_feedback_score, Some(1.5));
        assert_eq!(improved_skill.latest_average_feedback_score, Some(4.5));
        assert!(improved_skill.proposed_guardrails.is_empty());
    }

    #[test]
    fn doctor_separates_same_name_skills_by_skill_id() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("bug-triage")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            format!(
                "---\nname: bug-triage\nskill-id: {}\ndescription: triage bugs\n---\nOld revision\n",
                test_skill_id("bug-triage-old")
            ),
        )
        .expect("write original skill");
        let original_digest = file_digest(&skill_path);

        fs::write(
            &skill_path,
            format!(
                "---\nname: bug-triage\nskill-id: {}\ndescription: triage bugs\n---\nNew revision\n",
                test_skill_id("bug-triage-new")
            ),
        )
        .expect("rewrite skill");
        let recreated_digest = file_digest(&skill_path);

        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &[
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage-old"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: original_digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "old run one".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "bash".to_string(),
                        signature: "missing binary".to_string(),
                        message: "missing binary".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage-old"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: original_digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "old run two".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "bash".to_string(),
                        signature: "missing binary".to_string(),
                        message: "missing binary".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage-old"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: original_digest,
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "old run three".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "bash".to_string(),
                        signature: "missing binary".to_string(),
                        message: "missing binary".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:20:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage-new"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: recreated_digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "new run one".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage: SkillLineage::default(),
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:25:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage-new"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: recreated_digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "new run two".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage: SkillLineage::default(),
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:30:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage-new"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: recreated_digest,
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "new run three".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage: SkillLineage::default(),
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
            ],
        )
        .expect("write observations");

        let report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let old_skill = report
            .skills
            .iter()
            .find(|skill| skill.skill_id == test_skill_id("bug-triage-old"))
            .expect("old skill");
        let new_skill = report
            .skills
            .iter()
            .find(|skill| skill.skill_id == test_skill_id("bug-triage-new"))
            .expect("new skill");

        assert_eq!(old_skill.skill_name, "bug-triage");
        assert_eq!(old_skill.status, SkillHealthStatus::NeedsAmendment);
        assert_eq!(old_skill.current_digest, "missing");
        assert_eq!(new_skill.skill_name, "bug-triage");
        assert_eq!(new_skill.status, SkillHealthStatus::Healthy);
        assert_eq!(new_skill.current_digest, new_skill.latest_digest);
    }

    #[test]
    fn doctor_rolls_child_outcomes_up_to_creator_report() {
        let dir = tempdir().expect("tempdir");
        let healthy_skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("deploy-readiness")
            .join("SKILL.md");
        fs::create_dir_all(healthy_skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &healthy_skill_path,
            "---\nname: deploy-readiness\nskill-id: deploy-readiness-id\ndescription: check release readiness\nmetadata:\n  provenance:\n    created-by-skill: skill-creator\n    created-by-revision: rev-a\n    intended-outcome: catch release blockers before deploy\n    baseline: manual checklist review\n---\nReady\n",
        )
        .expect("write healthy skill");
        let failing_skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("release-notes")
            .join("SKILL.md");
        fs::create_dir_all(failing_skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &failing_skill_path,
            "---\nname: release-notes\nskill-id: release-notes-id\ndescription: draft release notes\nmetadata:\n  provenance:\n    created-by-skill: skill-creator\n    created-by-revision: rev-a\n    intended-outcome: produce accurate notes\n    baseline: manual note drafting\n---\nDraft\n",
        )
        .expect("write failing skill");

        let healthy_digest = file_digest(&healthy_skill_path);
        let failing_digest = file_digest(&failing_skill_path);
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &[
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("deploy-readiness"),
                    skill_name: "deploy-readiness".to_string(),
                    skill_path: healthy_skill_path.clone(),
                    skill_digest: healthy_digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "check this release candidate".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage: SkillLineage::default(),
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("deploy-readiness"),
                    skill_name: "deploy-readiness".to_string(),
                    skill_path: healthy_skill_path.clone(),
                    skill_digest: healthy_digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "audit this release branch".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage: SkillLineage::default(),
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("deploy-readiness"),
                    skill_name: "deploy-readiness".to_string(),
                    skill_path: healthy_skill_path.clone(),
                    skill_digest: healthy_digest,
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "run release readiness".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage: SkillLineage::default(),
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:15:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("release-notes"),
                    skill_name: "release-notes".to_string(),
                    skill_path: failing_skill_path.clone(),
                    skill_digest: failing_digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "draft release notes".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "write".to_string(),
                        signature: "missing release summary".to_string(),
                        message: "missing release summary".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:20:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("release-notes"),
                    skill_name: "release-notes".to_string(),
                    skill_path: failing_skill_path.clone(),
                    skill_digest: failing_digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "draft notes again".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "write".to_string(),
                        signature: "missing release summary".to_string(),
                        message: "missing release summary".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:25:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("release-notes"),
                    skill_name: "release-notes".to_string(),
                    skill_path: failing_skill_path.clone(),
                    skill_digest: failing_digest,
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "draft notes once more".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "write".to_string(),
                        signature: "missing release summary".to_string(),
                        message: "missing release summary".to_string(),
                    }],
                    agent_error: None,
                },
            ],
        )
        .expect("write observations");

        let report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let producer = report
            .producers
            .iter()
            .find(|producer| {
                producer.producer_skill_name == "skill-creator"
                    && producer.role == SkillProducerRole::Creator
            })
            .expect("creator report");
        assert_eq!(producer.producer_revision.as_deref(), Some("rev-a"));
        assert_eq!(producer.descendant_skill_count, 2);
        assert_eq!(producer.observed_descendant_skill_count, 2);
        assert_eq!(producer.orphaned_descendant_skill_count, 0);
        assert_eq!(producer.effective_descendant_skill_count, 1);
        assert_eq!(producer.needs_amendment_descendant_skill_count, 1);
        assert_eq!(producer.regressed_descendant_skill_count, 0);
    }

    #[test]
    fn doctor_attributes_observed_child_to_creator_and_improver() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("deploy-readiness")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: deploy-readiness\nskill-id: deploy-readiness-id\ndescription: check release readiness\nmetadata:\n  provenance:\n    created-by-skill: skill-creator\n    created-by-revision: rev-a\n    last-improved-by-skill: pi-skills-doctor\n    last-improved-by-revision: managed-patch-v1\n    intended-outcome: catch release blockers before deploy\n    baseline: manual checklist review\n---\nReady\n",
        )
        .expect("write skill");
        let digest = file_digest(&skill_path);
        let lineage = parse_skill_lineage(
            &parse_frontmatter(&fs::read_to_string(&skill_path).expect("read skill")).frontmatter,
        );
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &[
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("deploy-readiness"),
                    skill_name: "deploy-readiness".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "check this release candidate".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage: lineage.clone(),
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("deploy-readiness"),
                    skill_name: "deploy-readiness".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "audit this release branch".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage: lineage.clone(),
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("deploy-readiness"),
                    skill_name: "deploy-readiness".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest,
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "run release readiness".to_string(),
                    outcome: SkillRunOutcome::Success,
                    lineage,
                    tool_failures: Vec::new(),
                    agent_error: None,
                },
            ],
        )
        .expect("write observations");

        let report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        assert!(report.producers.iter().any(|producer| {
            producer.producer_skill_name == PI_SKILLS_DOCTOR_NAME
                && producer.role == SkillProducerRole::Improver
                && producer.descendant_skill_count == 1
        }));
        assert!(report.producers.iter().any(|producer| {
            producer.producer_skill_name == "skill-creator"
                && producer.role == SkillProducerRole::Creator
                && producer.descendant_skill_count == 1
        }));
    }

    #[test]
    fn doctor_keeps_orphaned_descendant_in_producer_report() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("release-notes")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: release-notes\nskill-id: release-notes-id\ndescription: draft release notes\nmetadata:\n  provenance:\n    created-by-skill: skill-creator\n    created-by-revision: rev-a\n---\nDraft\n",
        )
        .expect("write skill");
        let digest = file_digest(&skill_path);
        let lineage = parse_skill_lineage(
            &parse_frontmatter(&fs::read_to_string(&skill_path).expect("read skill")).frontmatter,
        );
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &[
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("release-notes"),
                    skill_name: "release-notes".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "draft notes".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: lineage.clone(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "write".to_string(),
                        signature: "missing summary".to_string(),
                        message: "missing summary".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("release-notes"),
                    skill_name: "release-notes".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "draft notes again".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: lineage.clone(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "write".to_string(),
                        signature: "missing summary".to_string(),
                        message: "missing summary".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("release-notes"),
                    skill_name: "release-notes".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest,
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "draft notes one more time".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage,
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "write".to_string(),
                        signature: "missing summary".to_string(),
                        message: "missing summary".to_string(),
                    }],
                    agent_error: None,
                },
            ],
        )
        .expect("write observations");
        fs::rename(
            &skill_path,
            skill_path
                .parent()
                .expect("parent")
                .join("SKILL.orphaned.md"),
        )
        .expect("rename orphaned skill");

        let report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let producer = report
            .producers
            .iter()
            .find(|producer| {
                producer.producer_skill_name == "skill-creator"
                    && producer.role == SkillProducerRole::Creator
            })
            .expect("creator report");
        assert_eq!(producer.descendant_skill_count, 1);
        assert_eq!(producer.observed_descendant_skill_count, 1);
        assert_eq!(producer.orphaned_descendant_skill_count, 1);
        assert!(
            producer
                .evidence
                .iter()
                .any(|entry| entry.contains("release-notes (release-notes-id)"))
        );
    }

    #[test]
    fn doctor_fails_loudly_on_malformed_observation_ledger() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".pi")).expect("create pi dir");
        fs::write(
            skill_observation_ledger_path(dir.path()),
            "{\"broken\": true\n",
        )
        .expect("write malformed ledger");

        let error = handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false)
            .expect_err("doctor should fail on malformed ledger");
        assert!(error.to_string().contains("failed to parse"));
    }

    #[test]
    fn doctor_fix_stamps_last_improved_lineage() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("bug-triage")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: bug-triage\nskill-id: bug-triage-id\ndescription: triage bugs\n---\nUse bash\n",
        )
        .expect("write skill");
        let digest = file_digest(&skill_path);
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &[
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "fix first".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "bash".to_string(),
                        signature: "cargo: command not found".to_string(),
                        message: "cargo: command not found".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest.clone(),
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "fix second".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "bash".to_string(),
                        signature: "cargo: command not found".to_string(),
                        message: "cargo: command not found".to_string(),
                    }],
                    agent_error: None,
                },
                SkillObservation {
                    version: 2,
                    recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                    session_id: None,
                    skill_id: test_skill_id("bug-triage"),
                    skill_name: "bug-triage".to_string(),
                    skill_path: skill_path.clone(),
                    skill_digest: digest,
                    activation_source: SkillActivationSource::SlashCommand,
                    task_preview: "fix third".to_string(),
                    outcome: SkillRunOutcome::ToolFailure,
                    lineage: SkillLineage::default(),
                    tool_failures: vec![SkillToolFailure {
                        tool_name: "bash".to_string(),
                        signature: "cargo: command not found".to_string(),
                        message: "cargo: command not found".to_string(),
                    }],
                    agent_error: None,
                },
            ],
        )
        .expect("write failing observations");

        let report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, true).expect("doctor fix");
        assert_eq!(report.applied_amendments.len(), 1);
        assert!(report.producers.iter().any(|producer| {
            producer.producer_skill_name == PI_SKILLS_DOCTOR_NAME
                && producer.role == SkillProducerRole::Improver
                && producer.descendant_skill_count == 1
        }));
        let raw = fs::read_to_string(&skill_path).expect("read amended skill");
        let parsed = parse_frontmatter(&raw);
        let lineage = parse_skill_lineage(&parsed.frontmatter);
        assert_eq!(
            lineage.last_improved_by_skill.as_deref(),
            Some(PI_SKILLS_DOCTOR_NAME)
        );
        assert_eq!(
            lineage.last_improved_by_revision.as_deref(),
            Some(PI_SKILLS_DOCTOR_REVISION)
        );
    }

    #[test]
    fn doctor_fix_rolls_back_regressed_managed_patch() {
        let dir = tempdir().expect("tempdir");
        let skill_path = dir
            .path()
            .join(".pi")
            .join("skills")
            .join("summarize")
            .join("SKILL.md");
        fs::create_dir_all(skill_path.parent().expect("parent")).expect("create skill dir");
        fs::write(
            &skill_path,
            "---\nname: summarize\nskill-id: summarize-id\ndescription: summarize text\n---\nReturn concise summaries.\n",
        )
        .expect("write skill");
        let original_digest = file_digest(&skill_path);
        let healthy_entries = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("summarize"),
                skill_name: "summarize".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "summarize first".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("summarize"),
                skill_name: "summarize".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "summarize second".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("summarize"),
                skill_name: "summarize".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: original_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "summarize third".to_string(),
                outcome: SkillRunOutcome::Success,
                lineage: SkillLineage::default(),
                tool_failures: Vec::new(),
                agent_error: None,
            },
        ];
        append_jsonl_records(&skill_observation_ledger_path(dir.path()), &healthy_entries)
            .expect("write healthy observations");

        let forward_amendment = apply_managed_skill_patch(
            "summarize",
            &skill_path,
            &["Always emit exactly three bullet points.".to_string()],
            &SkillRoutingHints {
                use_when: vec!["only when the user asks for concise summaries".to_string()],
                not_for: vec!["drafting original prose".to_string()],
                trigger_examples: vec!["summarize these meeting notes".to_string()],
                anti_trigger_examples: vec!["write a product announcement".to_string()],
            },
            &["manual test amendment".to_string()],
            dir.path(),
        )
        .expect("apply amendment")
        .expect("amendment");
        let amended_digest = forward_amendment.new_digest.clone();
        assert_eq!(forward_amendment.previous_guardrails, Vec::<String>::new());
        assert!(
            fs::read_to_string(&skill_path)
                .expect("read amended skill")
                .contains(ROUTING_BLOCK_BEGIN)
        );

        let regressed_entries = vec![
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:20:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("summarize"),
                skill_name: "summarize".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "summarize after amendment".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "write".to_string(),
                    signature: "unexpected output".to_string(),
                    message: "unexpected output".to_string(),
                }],
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:25:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("summarize"),
                skill_name: "summarize".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "summarize after amendment again".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "write".to_string(),
                    signature: "unexpected output".to_string(),
                    message: "unexpected output".to_string(),
                }],
                agent_error: None,
            },
            SkillObservation {
                version: 2,
                recorded_at_utc: "2026-03-15T10:30:00Z".to_string(),
                session_id: None,
                skill_id: test_skill_id("summarize"),
                skill_name: "summarize".to_string(),
                skill_path: skill_path.clone(),
                skill_digest: amended_digest.clone(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "summarize after amendment one more".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                lineage: SkillLineage::default(),
                tool_failures: vec![SkillToolFailure {
                    tool_name: "write".to_string(),
                    signature: "unexpected output".to_string(),
                    message: "unexpected output".to_string(),
                }],
                agent_error: None,
            },
        ];
        append_jsonl_records(
            &skill_observation_ledger_path(dir.path()),
            &regressed_entries,
        )
        .expect("write regressed observations");

        let regressed_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, false).expect("doctor");
        let regressed_skill = regressed_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "summarize")
            .expect("regressed skill report");
        assert_eq!(regressed_skill.status, SkillHealthStatus::Regressed);

        let rollback_report =
            handle_skill_doctor(dir.path(), SkillDoctorFormat::Json, true).expect("doctor fix");
        let rollback_skill = rollback_report
            .skills
            .iter()
            .find(|skill| skill.skill_name == "summarize")
            .expect("rollback skill report");
        assert_eq!(rollback_skill.status, SkillHealthStatus::PendingData);
        assert_eq!(rollback_skill.current_digest, file_digest(&skill_path));
        assert_ne!(rollback_skill.current_digest, amended_digest);
        assert_eq!(rollback_report.applied_amendments.len(), 1);
        let rolled_back = fs::read_to_string(&skill_path).expect("read rolled back skill");
        assert!(!rolled_back.contains(GUARDRAIL_BLOCK_BEGIN));
        assert!(!rolled_back.contains(ROUTING_BLOCK_BEGIN));
        let parsed = parse_frontmatter(&rolled_back);
        let lineage = parse_skill_lineage(&parsed.frontmatter);
        assert_eq!(
            lineage.last_improved_by_skill.as_deref(),
            Some(PI_SKILLS_DOCTOR_NAME)
        );
    }
}
