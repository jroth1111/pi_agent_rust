use super::schema::{ExplicitSkillInvocation, Skill};
use crate::agent::AgentEvent;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::extensions::sha256_hex_standalone;
use crate::session::Session;
use crate::tools::ToolOutput;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const SKILL_OBSERVATION_ENTRY_TYPE: &str = "skill/observation.v1";
pub const SKILL_AMENDMENT_ENTRY_TYPE: &str = "skill/amendment.v1";
const SKILL_OBSERVATION_LEDGER: &str = "skill-observations.jsonl";
const SKILL_AMENDMENT_LEDGER: &str = "skill-amendments.jsonl";
const GUARDRAIL_BLOCK_BEGIN: &str = "<!-- PI-SKILL-GUARDRAILS:BEGIN -->";
const GUARDRAIL_BLOCK_END: &str = "<!-- PI-SKILL-GUARDRAILS:END -->";

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
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub skill_digest: String,
    pub activation_source: SkillActivationSource,
    pub task_preview: String,
    pub outcome: SkillRunOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_failures: Vec<SkillToolFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillAmendment {
    pub version: u8,
    pub applied_at_utc: String,
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub previous_digest: String,
    pub new_digest: String,
    pub guardrails: Vec<String>,
    pub evidence: Vec<String>,
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
}

impl Default for SkillSuccessCriteria {
    fn default() -> Self {
        Self {
            min_sample_size: 3,
            min_success_rate: 0.8,
            max_consecutive_failures: 2,
            min_post_amend_runs: 3,
            min_improvement_delta: 0.15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillHealthReport {
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub latest_digest: String,
    pub run_count: usize,
    pub success_rate: f64,
    pub consecutive_failures: usize,
    pub status: SkillHealthStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed_guardrails: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_success_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_success_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDoctorReport {
    pub criteria: SkillSuccessCriteria,
    pub observation_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillHealthReport>,
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
    name: String,
    path: PathBuf,
    digest: String,
}

#[derive(Debug, Clone)]
struct ActivatedSkill {
    skill: TrackedSkill,
    source: SkillActivationSource,
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
                    name: skill.name.clone(),
                    path: skill.file_path.clone(),
                    digest: file_digest(&skill.file_path),
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
                name: skill_name.to_string(),
                path: path.to_path_buf(),
                digest: file_digest(path),
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
                version: 1,
                recorded_at_utc: timestamp_now(),
                session_id: self.session_id.clone(),
                skill_name: activation.skill.name.clone(),
                skill_path: activation.skill.path.clone(),
                skill_digest: activation.skill.digest.clone(),
                activation_source: activation.source,
                task_preview: self.task_preview.clone(),
                outcome,
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

pub fn handle_skill_doctor(
    cwd: &Path,
    format: SkillDoctorFormat,
    fix: bool,
) -> Result<SkillDoctorReport> {
    let criteria = SkillSuccessCriteria::default();
    let observations = load_observations(cwd)?;
    let mut grouped: BTreeMap<String, Vec<SkillObservation>> = BTreeMap::new();
    for observation in observations.iter().cloned() {
        grouped
            .entry(observation.skill_name.clone())
            .or_default()
            .push(observation);
    }

    let mut skills = Vec::new();
    for mut entries in grouped.into_values() {
        entries.sort_by(|a, b| a.recorded_at_utc.cmp(&b.recorded_at_utc));
        skills.push(build_skill_health_report(&entries, &criteria));
    }
    skills.sort_by(|a, b| a.skill_name.cmp(&b.skill_name));

    let mut applied_amendments = Vec::new();
    if fix {
        for report in &skills {
            if report.status != SkillHealthStatus::NeedsAmendment
                || report.proposed_guardrails.is_empty()
            {
                continue;
            }

            if let Some(amendment) = apply_guardrail_patch(
                &report.skill_name,
                &report.skill_path,
                &report.proposed_guardrails,
                &report.evidence,
                cwd,
            )? {
                applied_amendments.push(amendment);
            }
        }
    }

    let report = SkillDoctorReport {
        criteria,
        observation_count: observations.len(),
        skills,
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
    entries: &[SkillObservation],
    criteria: &SkillSuccessCriteria,
) -> SkillHealthReport {
    let latest = entries.last().expect("skill entries should not be empty");
    let run_count = entries.len();
    let success_rate = success_rate(entries);
    let consecutive_failures = entries
        .iter()
        .rev()
        .take_while(|entry| !entry.outcome.is_success())
        .count();
    let evidence = summarize_evidence(entries);
    let proposed_guardrails = recommend_guardrails(entries);

    let unhealthy = run_count >= criteria.min_sample_size
        && (success_rate < criteria.min_success_rate
            || consecutive_failures >= criteria.max_consecutive_failures);

    let (status, previous_success_rate, latest_success_rate) =
        evaluate_latest_revision(entries, criteria, unhealthy);

    SkillHealthReport {
        skill_name: latest.skill_name.clone(),
        skill_path: latest.skill_path.clone(),
        latest_digest: latest.skill_digest.clone(),
        run_count,
        success_rate,
        consecutive_failures,
        status,
        evidence,
        proposed_guardrails,
        previous_success_rate,
        latest_success_rate,
    }
}

fn evaluate_latest_revision(
    entries: &[SkillObservation],
    criteria: &SkillSuccessCriteria,
    unhealthy: bool,
) -> (SkillHealthStatus, Option<f64>, Option<f64>) {
    let latest_digest = &entries
        .last()
        .expect("skill entries should not be empty")
        .skill_digest;
    let split_at = entries
        .iter()
        .rposition(|entry| entry.skill_digest != *latest_digest)
        .map_or(0, |idx| idx + 1);

    if split_at == 0 {
        let status = if unhealthy {
            SkillHealthStatus::NeedsAmendment
        } else if entries.len() < criteria.min_sample_size {
            SkillHealthStatus::PendingData
        } else {
            SkillHealthStatus::Healthy
        };
        return (status, None, None);
    }

    let previous_entries = &entries[..split_at];
    let latest_entries = &entries[split_at..];
    let previous_rate = success_rate(previous_entries);
    let latest_rate = success_rate(latest_entries);

    if latest_entries.len() < criteria.min_post_amend_runs {
        let status = if unhealthy {
            SkillHealthStatus::NeedsAmendment
        } else {
            SkillHealthStatus::PendingData
        };
        return (status, Some(previous_rate), Some(latest_rate));
    }

    let delta = latest_rate - previous_rate;
    if delta >= criteria.min_improvement_delta {
        (
            SkillHealthStatus::Improved,
            Some(previous_rate),
            Some(latest_rate),
        )
    } else if delta <= -criteria.min_improvement_delta {
        (
            SkillHealthStatus::Regressed,
            Some(previous_rate),
            Some(latest_rate),
        )
    } else if unhealthy {
        (
            SkillHealthStatus::NeedsAmendment,
            Some(previous_rate),
            Some(latest_rate),
        )
    } else {
        (
            SkillHealthStatus::Healthy,
            Some(previous_rate),
            Some(latest_rate),
        )
    }
}

fn recommend_guardrails(entries: &[SkillObservation]) -> Vec<String> {
    let mut guardrails = Vec::new();
    let mut failure_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut agent_failure_count = 0usize;

    for entry in entries {
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
    if !entries.is_empty() && guardrails.is_empty() {
        guardrails.push(
            "Before each tool call, verify prerequisites and stop with a concrete explanation instead of cascading into secondary failures."
                .to_string(),
        );
    }

    guardrails
}

fn summarize_evidence(entries: &[SkillObservation]) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in entries {
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
        .collect()
}

fn apply_guardrail_patch(
    skill_name: &str,
    skill_path: &Path,
    guardrails: &[String],
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
    let block = guardrail_block(guardrails);
    let updated = replace_or_insert_guardrail_block(&raw, &block);
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
    let amendment = SkillAmendment {
        version: 1,
        applied_at_utc: timestamp_now(),
        skill_name: skill_name.to_string(),
        skill_path: skill_path.to_path_buf(),
        previous_digest,
        new_digest,
        guardrails: guardrails.to_vec(),
        evidence: evidence.to_vec(),
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

fn guardrail_block(guardrails: &[String]) -> String {
    let body = guardrails
        .iter()
        .map(|guardrail| format!("- {guardrail}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{GUARDRAIL_BLOCK_BEGIN}\n## Operational Guardrails\n{body}\n{GUARDRAIL_BLOCK_END}")
}

fn render_skill_doctor_report(report: &SkillDoctorReport) -> Result<String> {
    let mut out = String::new();
    writeln!(
        out,
        "Success criteria: min sample {}, target success rate {:.0}%, max consecutive failures {}, min post-amend runs {}, required improvement {:.0}%",
        report.criteria.min_sample_size,
        report.criteria.min_success_rate * 100.0,
        report.criteria.max_consecutive_failures,
        report.criteria.min_post_amend_runs,
        report.criteria.min_improvement_delta * 100.0,
    )
    .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
    writeln!(out, "Observed runs: {}", report.observation_count)
        .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;

    if report.skills.is_empty() {
        writeln!(out, "No skill observations recorded yet.")
            .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
    } else {
        for skill in &report.skills {
            writeln!(
                out,
                "{}: {:?} | runs={} success={:.0}% consecutive_failures={}",
                skill.skill_name,
                skill.status,
                skill.run_count,
                skill.success_rate * 100.0,
                skill.consecutive_failures
            )
            .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
            writeln!(out, "  path: {}", skill.skill_path.display())
                .map_err(|err| Error::session(format!("render skills doctor report: {err}")))?;
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
            if let (Some(previous), Some(latest)) =
                (skill.previous_success_rate, skill.latest_success_rate)
            {
                writeln!(
                    out,
                    "  evaluation: previous={:.0}% latest={:.0}%",
                    previous * 100.0,
                    latest * 100.0
                )
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
    let path = skill_observation_ledger_path(cwd);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(&path)
        .map_err(|err| Error::config(format!("failed to open {}: {err}", path.display())))?;
    let reader = BufReader::new(file);
    let mut observations = Vec::new();
    for line in reader.lines() {
        let line =
            line.map_err(|err| Error::config(format!("failed to read {}: {err}", path.display())))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<SkillObservation>(trimmed) {
            observations.push(record);
        }
    }
    Ok(observations)
}

fn skill_observation_ledger_path(cwd: &Path) -> PathBuf {
    cwd.join(Config::project_dir())
        .join(SKILL_OBSERVATION_LEDGER)
}

fn skill_amendment_ledger_path(cwd: &Path) -> PathBuf {
    cwd.join(Config::project_dir()).join(SKILL_AMENDMENT_LEDGER)
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
    use serde_json::json;
    use tempfile::tempdir;

    fn sample_tool_error(message: &str) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(message.to_string()))],
            details: None,
            is_error: true,
        }
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
            name: "bug-triage".to_string(),
            description: "triage bugs".to_string(),
            file_path: skill_path.clone(),
            base_dir: skill_path.parent().expect("parent").to_path_buf(),
            source: "project".to_string(),
            disable_model_invocation: false,
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
                version: 1,
                recorded_at_utc: "2026-03-15T10:00:00Z".to_string(),
                session_id: None,
                skill_name: "bug-triage".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
            SkillObservation {
                version: 1,
                recorded_at_utc: "2026-03-15T10:05:00Z".to_string(),
                session_id: None,
                skill_name: "bug-triage".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix again".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
            SkillObservation {
                version: 1,
                recorded_at_utc: "2026-03-15T10:10:00Z".to_string(),
                session_id: None,
                skill_name: "bug-triage".to_string(),
                skill_path: PathBuf::from("/tmp/SKILL.md"),
                skill_digest: "abc".to_string(),
                activation_source: SkillActivationSource::SlashCommand,
                task_preview: "fix again".to_string(),
                outcome: SkillRunOutcome::ToolFailure,
                tool_failures: vec![SkillToolFailure {
                    tool_name: "bash".to_string(),
                    signature: "cargo: command not found".to_string(),
                    message: "cargo: command not found".to_string(),
                }],
                agent_error: None,
            },
        ];
        let report = build_skill_health_report(&entries, &SkillSuccessCriteria::default());
        assert_eq!(report.status, SkillHealthStatus::NeedsAmendment);
        assert!(
            report
                .proposed_guardrails
                .iter()
                .any(|guardrail| guardrail.contains("Validate commands"))
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
            name: "summarize".to_string(),
            description: "summarize text".to_string(),
            file_path: skill_path.clone(),
            base_dir: skill_path.parent().expect("parent").to_path_buf(),
            source: "project".to_string(),
            disable_model_invocation: false,
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
}
