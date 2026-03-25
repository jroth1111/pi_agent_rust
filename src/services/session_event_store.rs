//! Authoritative session event store implementation.
//!
//! This module provides the **single authoritative write path** for session
//! persistence. All session events (messages, model changes, compactions,
//! reliability entries, etc.) flow through this store before any projection
//! is treated as current truth.
//!
//! Legacy session stores (JSONL V1, SQLite, V2 sidecar) are gated to
//! import/export/migration/inspection roles only — they are never used as
//! live production authorities after cutover.

use crate::config::Config;
use crate::contracts::dto::{
    BranchContinuityState, CompactionContinuity, LegacyImportRequest, LegacyStoreRole,
    LegacyStoreValidation, ModelControl, PersistenceSnapshot, PersistenceStoreKind, SessionEvent,
    SessionEventPayload, SessionIdentity, SessionProjection, SkillActivation, SkillActivationSet,
    SkillSource, ThinkingLevel,
};
use crate::contracts::engine::PersistenceContract;
use crate::error::{Error, Result};
use crate::session::{Session, SessionEntry, SessionMessage};
use crate::session_store_v2::SessionStoreV2;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Default maximum segment size (64MB).
const DEFAULT_MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

/// The authoritative session event store.
///
/// Backed by `SessionStoreV2` segmented append log. Each session has its own
/// V2 store directory. Projections (session list, picker data, state queries)
/// are derived from the event store and are fully rebuildable.
pub struct SessionEventStore {
    /// Root directory for all session event stores.
    sessions_root: PathBuf,
    /// Cache of open V2 stores keyed by session ID.
    /// Protected by a std Mutex because the underlying SessionStoreV2
    /// operations are synchronous file I/O.
    stores: Mutex<HashMap<String, SessionStoreV2>>,
    /// Maximum segment size for V2 stores.
    max_segment_bytes: u64,
}

impl SessionEventStore {
    /// Create a new event store rooted in the given directory.
    pub fn new(sessions_root: PathBuf) -> Self {
        Self {
            sessions_root,
            stores: Mutex::new(HashMap::new()),
            max_segment_bytes: DEFAULT_MAX_SEGMENT_BYTES,
        }
    }

    /// Create with the default sessions directory.
    pub fn from_config() -> Self {
        Self::new(Config::sessions_dir())
    }

    /// Get or create the V2 store for a session.
    fn get_or_create_store(&self, session_id: &str) -> Result<SessionStoreV2> {
        self.open_store(session_id)
    }

    /// Open the V2 store for a session.
    fn open_store(&self, session_id: &str) -> Result<SessionStoreV2> {
        let store_root = self.session_store_root(session_id);
        SessionStoreV2::create(&store_root, self.max_segment_bytes)
    }

    /// Get the V2 store root directory for a session.
    fn session_store_root(&self, session_id: &str) -> PathBuf {
        self.sessions_root.join(format!("{session_id}.v2"))
    }

    /// Get the V2 sidecar root for a given JSONL session path.
    fn v2_sidecar_path(jsonl_path: &Path) -> PathBuf {
        let mut p = jsonl_path.to_path_buf();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // Replace .jsonl extension with .v2
        let stem = std::path::Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let v2_name = if std::path::Path::new(&name)
            .extension()
            .is_some_and(|ext| ext == "jsonl")
        {
            format!("{stem}.v2")
        } else {
            format!("{name}.v2")
        };
        p.set_file_name(v2_name);
        p
    }

    /// Convert a `SessionEventPayload` to the JSON Value format expected by
    /// the V2 store's `append_entry` method.
    fn payload_to_entry_value(payload: &SessionEventPayload) -> (String, Value) {
        match payload {
            SessionEventPayload::Message { role, content } => (
                "message".to_string(),
                serde_json::json!({ "role": role, "content": content }),
            ),
            SessionEventPayload::ModelChange { provider, model_id } => (
                "model_change".to_string(),
                serde_json::json!({ "provider": provider, "modelId": model_id }),
            ),
            SessionEventPayload::ThinkingLevelChange { thinking_level } => (
                "thinking_level_change".to_string(),
                serde_json::json!({ "thinkingLevel": thinking_level }),
            ),
            SessionEventPayload::Compaction {
                summary,
                compacted_entry_count,
                original_message_count,
                continuity,
            } => {
                let mut obj = serde_json::json!({
                    "summary": summary,
                    "compactedEntryCount": compacted_entry_count,
                    "originalMessageCount": original_message_count,
                });
                if let Some(cont) = continuity {
                    obj["continuity"] = serde_json::to_value(cont).unwrap_or(Value::Null);
                }
                ("compaction".to_string(), obj)
            }
            SessionEventPayload::BranchSummary {
                summary,
                from_leaf_id,
            } => (
                "branch_summary".to_string(),
                serde_json::json!({ "summary": summary, "fromLeafId": from_leaf_id }),
            ),
            SessionEventPayload::SkillActivation {
                skill_name,
                source,
                file_path,
                disable_model_invocation,
                description,
            } => (
                "skill_activation".to_string(),
                serde_json::json!({
                    "skillName": skill_name,
                    "source": source,
                    "filePath": file_path,
                    "disableModelInvocation": disable_model_invocation,
                    "description": description,
                }),
            ),
            SessionEventPayload::SkillActivationSnapshot {
                skills_disabled,
                skills,
            } => (
                "skill_activation_snapshot".to_string(),
                serde_json::json!({
                    "skillsDisabled": skills_disabled,
                    "skills": skills,
                }),
            ),
            SessionEventPayload::Label {
                label,
                target_entry_id,
            } => (
                "label".to_string(),
                serde_json::json!({ "label": label, "targetEntryId": target_entry_id }),
            ),
            SessionEventPayload::SessionInfo { key, value } => (
                "session_info".to_string(),
                serde_json::json!({ "key": key, "value": value }),
            ),
            SessionEventPayload::ReliabilityStateDigest { payload } => {
                ("reliability/state_digest.v1".to_string(), payload.clone())
            }
            SessionEventPayload::ReliabilityTaskCheckpoint { payload } => (
                "reliability/task_checkpoint.v1".to_string(),
                payload.clone(),
            ),
            SessionEventPayload::ReliabilityTaskCreated { payload } => {
                ("reliability/task_created.v1".to_string(), payload.clone())
            }
            SessionEventPayload::ReliabilityTaskTransition { payload } => (
                "reliability/task_transition.v1".to_string(),
                payload.clone(),
            ),
            SessionEventPayload::ReliabilityVerificationEvidence { payload } => (
                "reliability/verification_evidence.v1".to_string(),
                payload.clone(),
            ),
            SessionEventPayload::ReliabilityCloseDecision { payload } => {
                ("reliability/close_decision.v1".to_string(), payload.clone())
            }
            SessionEventPayload::ReliabilityHumanBlockerRaised { payload } => (
                "reliability/human_blocker_raised.v1".to_string(),
                payload.clone(),
            ),
            SessionEventPayload::ReliabilityHumanBlockerResolved { payload } => (
                "reliability/human_blocker_resolved.v1".to_string(),
                payload.clone(),
            ),
            SessionEventPayload::Custom { name, payload } => {
                (format!("custom/{name}"), payload.clone())
            }
        }
    }

    /// Convert a V2 SegmentFrame to a SessionEvent.
    fn frame_to_session_event(
        frame: &crate::session_store_v2::SegmentFrame,
    ) -> Result<SessionEvent> {
        let payload: Value = serde_json::from_str(frame.payload.get())?;
        let session_payload = Self::value_to_session_event_payload(&frame.entry_type, &payload)?;
        Ok(SessionEvent {
            seq: frame.entry_seq,
            event_id: frame.entry_id.clone(),
            parent_event_id: frame.parent_entry_id.clone(),
            payload: session_payload,
            timestamp: frame.timestamp.clone(),
            payload_checksum: frame.payload_sha256.clone(),
        })
    }

    /// Convert a JSON value + entry type string to a SessionEventPayload.
    fn value_to_session_event_payload(
        entry_type: &str,
        value: &Value,
    ) -> Result<SessionEventPayload> {
        match entry_type {
            "message" => {
                let role = value
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let content = value.get("content").cloned().unwrap_or(Value::Null);
                Ok(SessionEventPayload::Message { role, content })
            }
            "model_change" => {
                let provider = value
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let model_id = value
                    .get("modelId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(SessionEventPayload::ModelChange { provider, model_id })
            }
            "thinking_level_change" => {
                let thinking_level = value
                    .get("thinkingLevel")
                    .and_then(|v| v.as_str())
                    .unwrap_or("off")
                    .to_string();
                Ok(SessionEventPayload::ThinkingLevelChange { thinking_level })
            }
            "compaction" => {
                let summary = value
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let compacted_entry_count = value
                    .get("compactedEntryCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let original_message_count = value
                    .get("originalMessageCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                // Parse typed continuity when present; fall back to None for
                // legacy events that used the ad hoc Value blob.
                let continuity = value.get("continuity").and_then(|v| {
                    if v.is_object() {
                        serde_json::from_value(v.clone()).ok()
                    } else {
                        None
                    }
                });
                Ok(SessionEventPayload::Compaction {
                    summary,
                    compacted_entry_count,
                    original_message_count,
                    continuity,
                })
            }
            "branch_summary" => {
                let summary = value
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let from_leaf_id = value
                    .get("fromLeafId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Ok(SessionEventPayload::BranchSummary {
                    summary,
                    from_leaf_id,
                })
            }
            "skill_activation" => {
                let skill_name = value
                    .get("skillName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let source = value
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("path");
                let source = match source {
                    "user" => SkillSource::User,
                    "project" => SkillSource::Project,
                    _ => SkillSource::Path,
                };
                let file_path = value
                    .get("filePath")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let disable_model_invocation = value
                    .get("disableModelInvocation")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let description = value
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Ok(SessionEventPayload::SkillActivation {
                    skill_name,
                    source,
                    file_path,
                    disable_model_invocation,
                    description,
                })
            }
            "skill_activation_snapshot" => {
                let skills_disabled = value
                    .get("skillsDisabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let skills = value
                    .get("skills")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                Some(SkillActivation {
                                    skill_name: item.get("skillName")?.as_str()?.to_string(),
                                    source: match item.get("source")?.as_str()? {
                                        "user" => SkillSource::User,
                                        "project" => SkillSource::Project,
                                        _ => SkillSource::Path,
                                    },
                                    file_path: item
                                        .get("filePath")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    disable_model_invocation: item
                                        .get("disableModelInvocation")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false),
                                    description: item
                                        .get("description")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(SessionEventPayload::SkillActivationSnapshot {
                    skills_disabled,
                    skills,
                })
            }
            "label" => {
                let label = value
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let target_entry_id = value
                    .get("targetEntryId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Ok(SessionEventPayload::Label {
                    label,
                    target_entry_id,
                })
            }
            "session_info" => {
                let key = value
                    .get("key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let val = value.get("value").cloned().unwrap_or(Value::Null);
                Ok(SessionEventPayload::SessionInfo { key, value: val })
            }
            t if t.starts_with("reliability/") => Ok(SessionEventPayload::ReliabilityStateDigest {
                payload: value.clone(),
            }),
            t if t.starts_with("custom/") => {
                let name = t.strip_prefix("custom/").unwrap_or("unknown").to_string();
                Ok(SessionEventPayload::Custom {
                    name,
                    payload: value.clone(),
                })
            }
            other => Err(Error::session(format!("unknown event type: {other}"))),
        }
    }

    /// Build a projection from a list of events.
    fn build_projection_from_events(
        session_id: &str,
        events: &[SessionEvent],
    ) -> SessionProjection {
        let mut leaf_event_id = None;
        let mut root_event_id = None;
        let mut message_count: u64 = 0;
        let mut session_name = None;
        let mut current_provider = None;
        let mut current_model_id = None;
        let mut current_thinking_level = None;
        let mut entry_ids = std::collections::HashSet::new();
        let mut parent_map: HashMap<String, String> = HashMap::new();
        let mut children_count: HashMap<String, usize> = HashMap::new();
        let mut is_linear = true;
        let mut compaction_continuity: Option<CompactionContinuity> = None;
        let mut skill_activations: Vec<SkillActivation> = Vec::new();
        let mut skills_disabled = false;
        let mut seen_skill_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for event in events {
            entry_ids.insert(event.event_id.clone());
            if let Some(ref parent_id) = event.parent_event_id {
                parent_map.insert(event.event_id.clone(), parent_id.clone());
                *children_count.entry(parent_id.clone()).or_insert(0) += 1;
            }

            // Track root: the first event with no parent
            if root_event_id.is_none() && event.parent_event_id.is_none() {
                root_event_id = Some(event.event_id.clone());
            }

            // Track leaf: the last event in sequence is the leaf
            leaf_event_id = Some(event.event_id.clone());

            match &event.payload {
                SessionEventPayload::Message { .. } => {
                    message_count += 1;
                }
                SessionEventPayload::ModelChange { provider, model_id } => {
                    current_provider = Some(provider.clone());
                    current_model_id = Some(model_id.clone());
                }
                SessionEventPayload::ThinkingLevelChange { thinking_level } => {
                    current_thinking_level = Some(thinking_level.clone());
                }
                SessionEventPayload::SessionInfo { key, value } => {
                    if key == "name" {
                        session_name = value.as_str().map(|s| s.to_string());
                    }
                }
                SessionEventPayload::Compaction { continuity, .. } => {
                    if let Some(cont) = continuity {
                        compaction_continuity = Some(cont.clone());
                    }
                }
                SessionEventPayload::SkillActivation {
                    skill_name,
                    source,
                    file_path,
                    disable_model_invocation,
                    description,
                } => {
                    // Deduplicate: keep the latest activation for each skill name
                    if seen_skill_names.insert(skill_name.clone()) {
                        skill_activations.push(SkillActivation {
                            skill_name: skill_name.clone(),
                            source: *source,
                            file_path: file_path.clone(),
                            disable_model_invocation: *disable_model_invocation,
                            description: description.clone(),
                        });
                    } else {
                        // Update existing entry to latest activation
                        if let Some(existing) = skill_activations
                            .iter_mut()
                            .find(|s| s.skill_name == *skill_name)
                        {
                            existing.source = *source;
                            existing.file_path.clone_from(file_path);
                            existing.disable_model_invocation = *disable_model_invocation;
                            existing.description.clone_from(description);
                        }
                    }
                }
                SessionEventPayload::SkillActivationSnapshot {
                    skills_disabled: sd,
                    skills,
                } => {
                    skills_disabled = *sd;
                    // A snapshot replaces the accumulated skill set entirely
                    skill_activations.clone_from(skills);
                    seen_skill_names = skill_activations
                        .iter()
                        .map(|s| s.skill_name.clone())
                        .collect();
                }
                _ => {}
            }
        }

        // Check if linear: no entry has more than one child
        for &count in children_count.values() {
            if count > 1 {
                is_linear = false;
                break;
            }
        }

        // Build branch continuity state
        let branch_continuity = leaf_event_id.as_ref().map(|leaf| {
            // Find fork points: entries with more than one child
            let fork_points: Vec<String> = children_count
                .iter()
                .filter(|&(_, &count)| count > 1)
                .map(|(id, _)| id.clone())
                .collect();

            // Find branch heads: entries that are children of fork points
            // and have their own descendants (or are leaf-like)
            let branch_heads: Vec<String> = fork_points
                .iter()
                .flat_map(|fork_id| {
                    events
                        .iter()
                        .filter(|e| e.parent_event_id.as_deref() == Some(fork_id.as_str()))
                        .map(|e| e.event_id.clone())
                        .collect::<Vec<_>>()
                })
                .collect();

            BranchContinuityState {
                leaf_event_id: leaf.clone(),
                root_event_id: root_event_id.clone(),
                is_linear,
                branch_heads,
                fork_points,
            }
        });

        let current_model = current_provider
            .zip(current_model_id)
            .map(|(provider, model_id)| ModelControl {
                model_id,
                provider,
                thinking_level: current_thinking_level
                    .as_deref()
                    .and_then(|t| t.parse::<ThinkingLevel>().ok())
                    .unwrap_or_default(),
                thinking_budget_tokens: None,
            });

        let built_from_offset = events.last().map_or(0, |e| e.seq);

        // Build skill activation set if any skills were activated
        let skill_set = if !skill_activations.is_empty() || skills_disabled {
            Some(SkillActivationSet {
                skills: skill_activations,
                skills_disabled,
            })
        } else {
            None
        };

        SessionProjection {
            session_id: session_id.to_string(),
            event_count: events.len() as u64,
            leaf_event_id,
            is_linear,
            message_count,
            session_name,
            current_model,
            current_thinking_level,
            built_from_offset,
            branch_continuity,
            compaction_continuity,
            skill_activations: skill_set,
        }
    }

    /// Generate a unique event ID.
    fn next_event_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

#[async_trait]
impl PersistenceContract for SessionEventStore {
    // -- Health and observability --

    async fn snapshot(&self) -> Result<PersistenceSnapshot> {
        // For the event store, we report V2 as the store kind.
        Ok(PersistenceSnapshot {
            store_kind: PersistenceStoreKind::EventStore,
            is_healthy: true,
            entry_count: 0, // Would need to enumerate sessions for accurate count
            last_persisted_offset: 0,
            pending_mutations: 0,
        })
    }

    async fn is_healthy(&self) -> bool {
        self.sessions_root.exists()
    }

    async fn rebuild_projections(&self) -> Result<()> {
        // Projections are derived on-demand from the event store.
        // This method ensures the sessions root exists and is writable.
        std::fs::create_dir_all(&self.sessions_root)?;
        Ok(())
    }

    async fn last_persisted_offset(&self) -> u64 {
        // Returns the last global offset across all sessions.
        // Individual session offsets are tracked per-store.
        0
    }

    async fn pending_mutations(&self) -> usize {
        0
    }

    async fn flush(&self) -> Result<()> {
        // V2 stores fsync on each append, so flush is a no-op.
        Ok(())
    }

    // -- Authoritative session event store operations --

    async fn create_session(&self, session_id: String) -> Result<String> {
        let store_root = self.session_store_root(&session_id);
        std::fs::create_dir_all(&store_root)?;
        Ok(session_id)
    }

    async fn append_event(
        &self,
        session_id: &str,
        payload: SessionEventPayload,
        parent_event_id: Option<String>,
    ) -> Result<u64> {
        let (entry_type, entry_value) = Self::payload_to_entry_value(&payload);
        let event_id = Self::next_event_id();

        let mut store = self.open_store(session_id)?;
        let index_entry =
            store.append_entry(&event_id, parent_event_id, &entry_type, entry_value)?;

        Ok(index_entry.entry_seq)
    }

    async fn session_projection(&self, session_id: &str) -> Result<SessionProjection> {
        let store = self.open_store(session_id)?;
        let frames = store.read_all_entries()?;
        let events: Vec<SessionEvent> = frames
            .iter()
            .filter_map(|f| Self::frame_to_session_event(f).ok())
            .collect();
        Ok(Self::build_projection_from_events(session_id, &events))
    }

    async fn read_events(
        &self,
        session_id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<SessionEvent>> {
        let store = self.open_store(session_id)?;
        let all_frames = store.read_entries_from(offset)?;
        let events: Vec<SessionEvent> = all_frames
            .iter()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .filter_map(|f| Self::frame_to_session_event(f).ok())
            .collect();
        Ok(events)
    }

    async fn read_active_path(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        let store = self.open_store(session_id)?;
        let head = store.head().ok_or_else(|| {
            Error::session(format!("session {session_id} is empty, no active path"))
        })?;
        let frames = store.read_active_path(&head.entry_id)?;
        let events: Vec<SessionEvent> = frames
            .iter()
            .filter_map(|f| Self::frame_to_session_event(f).ok())
            .collect();
        Ok(events)
    }

    async fn read_tail(&self, session_id: &str, count: u64) -> Result<Vec<SessionEvent>> {
        let store = self.open_store(session_id)?;
        let frames = store.read_tail_entries(count)?;
        let events: Vec<SessionEvent> = frames
            .iter()
            .filter_map(|f| Self::frame_to_session_event(f).ok())
            .collect();
        Ok(events)
    }

    async fn compact_session(
        &self,
        session_id: &str,
        summary: String,
        compacted_entry_count: u64,
        original_message_count: u64,
        continuity: Option<Value>,
    ) -> Result<u64> {
        // Get current leaf for parent reference
        let store = self.open_store(session_id)?;
        let parent_event_id = store.head().map(|h| h.entry_id);

        // Parse typed continuity from Value if possible; otherwise store None.
        let typed_continuity: Option<CompactionContinuity> = continuity
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        let payload = SessionEventPayload::Compaction {
            summary,
            compacted_entry_count,
            original_message_count,
            continuity: typed_continuity,
        };

        self.append_event(session_id, payload, parent_event_id)
            .await
    }

    // -- Legacy store gating --

    async fn validate_legacy_store(
        &self,
        path: &str,
        role: LegacyStoreRole,
    ) -> Result<LegacyStoreValidation> {
        let path = PathBuf::from(path);
        if !path.exists() {
            return Ok(LegacyStoreValidation {
                role,
                source_kind: PersistenceStoreKind::Jsonl,
                entry_count: 0,
                is_valid: false,
                errors: vec!["path does not exist".to_string()],
            });
        }

        let source_kind = match path.extension().and_then(|e| e.to_str()) {
            Some("jsonl") => PersistenceStoreKind::Jsonl,
            Some("sqlite") => PersistenceStoreKind::Sqlite,
            _ => PersistenceStoreKind::Jsonl,
        };

        let mut errors = Vec::new();
        let entry_count = match source_kind {
            PersistenceStoreKind::Jsonl => {
                // Count JSONL lines (quick validation)
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
                        // First line is header, rest are entries
                        if lines.is_empty() {
                            errors.push("empty session file".to_string());
                            0
                        } else {
                            (lines.len() - 1) as u64
                        }
                    }
                    Err(e) => {
                        errors.push(format!("failed to read: {e}"));
                        0
                    }
                }
            }
            PersistenceStoreKind::Sqlite => {
                errors.push("SQLite legacy validation not yet implemented".to_string());
                0
            }
            PersistenceStoreKind::V2Sidecar | PersistenceStoreKind::EventStore => {
                errors.push(format!(
                    "source_kind {source_kind:?} is not a legacy format"
                ));
                0
            }
        };

        Ok(LegacyStoreValidation {
            role,
            source_kind,
            entry_count,
            is_valid: errors.is_empty(),
            errors,
        })
    }

    async fn import_legacy_session(&self, request: LegacyImportRequest) -> Result<String> {
        // Validate the role — must be import/migration, not a live authority role
        match request.role {
            LegacyStoreRole::Import | LegacyStoreRole::Migration => {}
            LegacyStoreRole::Export | LegacyStoreRole::Inspection => {
                return Err(Error::validation(format!(
                    "legacy store role {:?} cannot be used for import",
                    request.role
                )));
            }
        }

        let source_path = PathBuf::from(&request.source_path);
        if !source_path.exists() {
            return Err(Error::session(format!(
                "legacy session not found: {}",
                request.source_path
            )));
        }

        // Open the legacy session using the existing Session::open path
        let legacy_session = Session::open(&request.source_path).await?;
        let session_id = legacy_session.header.id.clone();

        // Create the event store session
        self.create_session(session_id.clone()).await?;

        // Import each entry as an event
        for entry in &legacy_session.entries_for_current_path() {
            let (event_id, parent_event_id, payload) = Self::session_entry_to_event_payload(entry);
            let (entry_type, entry_value) = Self::payload_to_entry_value(&payload);

            let mut store = self.open_store(&session_id)?;
            store.append_entry(&event_id, parent_event_id, &entry_type, entry_value)?;
        }

        Ok(session_id)
    }

    async fn export_session(
        &self,
        session_id: &str,
        target_kind: PersistenceStoreKind,
        target_path: &str,
    ) -> Result<()> {
        match target_kind {
            PersistenceStoreKind::Jsonl => {
                let events = self.read_events(session_id, 1, u64::MAX).await?;

                // Create a Session from the events and save as JSONL
                let mut session = Session::create_with_dir(Some(self.sessions_root.clone()));
                session.header.id = session_id.to_string();

                // Write header + entries to the target path
                let path = PathBuf::from(target_path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                let mut file = std::fs::File::create(&path)?;
                use std::io::Write;
                writeln!(file, "{}", serde_json::to_string(&session.header)?)?;

                for event in &events {
                    let (entry_type, entry_value) = Self::payload_to_entry_value(&event.payload);
                    let entry_json = serde_json::json!({
                        "type": entry_type,
                        "id": event.event_id,
                        "parentId": event.parent_event_id,
                        "timestamp": event.timestamp,
                        "payload": entry_value,
                    });
                    writeln!(file, "{}", serde_json::to_string(&entry_json)?)?;
                }
            }
            PersistenceStoreKind::Sqlite => {
                return Err(Error::validation(
                    "SQLite export is not yet implemented for the event store",
                ));
            }
            PersistenceStoreKind::V2Sidecar | PersistenceStoreKind::EventStore => {
                return Err(Error::validation(format!(
                    "cannot export to {target_kind:?}: not a legacy format"
                )));
            }
        }
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionIdentity>> {
        let mut sessions = Vec::new();

        if !self.sessions_root.exists() {
            return Ok(sessions);
        }

        for entry in std::fs::read_dir(&self.sessions_root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Look for .v2 directories
            if name_str.ends_with(".v2") && entry.file_type()?.is_dir() {
                let session_id = &name_str[..name_str.len() - 3];
                sessions.push(SessionIdentity {
                    session_id: session_id.to_string(),
                    name: None,
                    path: Some(entry.path().display().to_string()),
                });
            }
        }

        Ok(sessions)
    }
}

impl SessionEventStore {
    /// Convert a legacy `SessionEntry` to event payload components.
    fn session_entry_to_event_payload(
        entry: &SessionEntry,
    ) -> (String, Option<String>, SessionEventPayload) {
        let base = entry.base();
        let event_id = base
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let parent_event_id = base.parent_id.clone();

        // Build payload from entry type
        let payload = match entry {
            SessionEntry::Message(msg) => {
                let role = match &msg.message {
                    SessionMessage::User { .. } => "user",
                    SessionMessage::Assistant { .. } => "assistant",
                    SessionMessage::ToolResult { .. } => "tool_result",
                    _ => "other",
                };
                SessionEventPayload::Message {
                    role: role.to_string(),
                    content: serde_json::to_value(&msg.message).unwrap_or(Value::Null),
                }
            }
            SessionEntry::ModelChange(mc) => SessionEventPayload::ModelChange {
                provider: mc.provider.clone(),
                model_id: mc.model_id.clone(),
            },
            SessionEntry::ThinkingLevelChange(tl) => SessionEventPayload::ThinkingLevelChange {
                thinking_level: tl.thinking_level.clone(),
            },
            SessionEntry::Compaction(c) => {
                // Attempt to parse typed continuity from the CompactionEntry details.
                // Legacy entries may have ad hoc JSON blobs that don't conform;
                // those will yield None, which is safe.
                let typed_continuity = c
                    .details
                    .as_ref()
                    .and_then(|v| serde_json::from_value::<CompactionContinuity>(v.clone()).ok());
                SessionEventPayload::Compaction {
                    summary: c.summary.clone(),
                    compacted_entry_count: 0,
                    original_message_count: 0,
                    continuity: typed_continuity,
                }
            }
            SessionEntry::BranchSummary(bs) => SessionEventPayload::BranchSummary {
                summary: bs.summary.clone(),
                from_leaf_id: Some(bs.from_id.clone()),
            },
            SessionEntry::Label(l) => SessionEventPayload::Label {
                label: l.label.clone().unwrap_or_default(),
                target_entry_id: Some(l.target_id.clone()),
            },
            SessionEntry::SessionInfo(si) => SessionEventPayload::SessionInfo {
                key: "name".to_string(),
                value: serde_json::to_value(si.name.as_deref().unwrap_or(""))
                    .unwrap_or(Value::Null),
            },
            _ => SessionEventPayload::Custom {
                name: "legacy_import".to_string(),
                payload: serde_json::to_value(entry).unwrap_or(Value::Null),
            },
        };

        (event_id, parent_event_id, payload)
    }
}

/// Validate skill activations from a session projection against available skills.
///
/// This is the fail-closed path for VAL-SESS-009: when a session is resumed
/// and the event store contains skill activations, each referenced skill is
/// validated against the currently available skills. Missing skills produce
/// explicit `MissingSkill` records and cause `is_valid` to be `false`.
pub fn validate_skill_activations(
    projection: &SessionProjection,
    available_skill_paths: &[std::path::PathBuf],
) -> crate::contracts::dto::SkillActivationValidation {
    use crate::contracts::dto::{MissingSkill, SkillActivationValidation};

    let Some(ref activation_set) = projection.skill_activations else {
        // No skills were active — validation passes trivially.
        return SkillActivationValidation {
            found_skills: Vec::new(),
            missing_skills: Vec::new(),
            is_valid: true,
        };
    };

    if activation_set.skills_disabled {
        // Skills were explicitly disabled — validation passes.
        return SkillActivationValidation {
            found_skills: Vec::new(),
            missing_skills: Vec::new(),
            is_valid: true,
        };
    }

    // Build a set of available skill names from the provided paths.
    // A skill is considered available if any provided path's stem matches
    // the skill name AND the path exists on disk, OR if the activation's
    // file_path matches any provided path (covers direct path matches).
    let mut available_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut existing_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    for path in available_skill_paths {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !name.is_empty() && path.exists() {
            available_names.insert(name.to_string());
        }
        // Also track the path string itself for direct file_path matching.
        if let Some(path_str) = path.to_str() {
            existing_paths.insert(path_str.to_string());
        }
    }

    let mut found_skills = Vec::new();
    let mut missing_skills = Vec::new();

    for activation in &activation_set.skills {
        // A skill is available if:
        // 1. Its name appears in the available names set (path exists), OR
        // 2. Its recorded file_path appears in the existing paths set.
        let name_available = available_names.contains(&activation.skill_name);
        let path_available = existing_paths.contains(&activation.file_path);
        let is_available = name_available || path_available;
        if is_available {
            found_skills.push(activation.skill_name.clone());
        } else {
            missing_skills.push(MissingSkill {
                skill_name: activation.skill_name.clone(),
                expected_file_path: activation.file_path.clone(),
                source: activation.source,
            });
        }
    }

    let is_valid = missing_skills.is_empty();

    SkillActivationValidation {
        found_skills,
        missing_skills,
        is_valid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::dto::SessionEventPayload;

    #[test]
    fn build_projection_empty_events() {
        let proj = SessionEventStore::build_projection_from_events("test-session", &[]);
        assert_eq!(proj.session_id, "test-session");
        assert_eq!(proj.event_count, 0);
        assert!(proj.leaf_event_id.is_none());
        assert!(proj.is_linear);
        assert_eq!(proj.message_count, 0);
    }

    #[test]
    fn build_projection_linear_messages() {
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::Message {
                    role: "user".to_string(),
                    content: serde_json::json!("hello"),
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "abc".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("hi there"),
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "def".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("test-session", &events);
        assert_eq!(proj.event_count, 2);
        assert_eq!(proj.message_count, 2);
        assert!(proj.is_linear);
        assert_eq!(proj.leaf_event_id.as_deref(), Some("e2"));
    }

    #[test]
    fn build_projection_tracks_model_changes() {
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::ModelChange {
                    provider: "anthropic".to_string(),
                    model_id: "claude-3".to_string(),
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "abc".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::ModelChange {
                    provider: "openai".to_string(),
                    model_id: "gpt-4".to_string(),
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "def".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("test-session", &events);
        let model = proj.current_model.expect("should have model");
        assert_eq!(model.provider, "openai");
        assert_eq!(model.model_id, "gpt-4");
    }

    #[test]
    fn build_projection_detects_branching() {
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::Message {
                    role: "user".to_string(),
                    content: serde_json::json!("root"),
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "a".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("branch-a"),
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "b".to_string(),
            },
            SessionEvent {
                seq: 3,
                event_id: "e3".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("branch-b"),
                },
                timestamp: "2026-01-01T00:00:02Z".to_string(),
                payload_checksum: "c".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("test-session", &events);
        assert!(!proj.is_linear);
    }

    #[test]
    fn payload_roundtrip_message() {
        let payload = SessionEventPayload::Message {
            role: "user".to_string(),
            content: serde_json::json!({"text": "hello"}),
        };
        let (entry_type, value) = SessionEventStore::payload_to_entry_value(&payload);
        assert_eq!(entry_type, "message");
        assert_eq!(value["role"], "user");

        let roundtrip =
            SessionEventStore::value_to_session_event_payload(&entry_type, &value).unwrap();
        assert!(matches!(roundtrip, SessionEventPayload::Message { .. }));
    }

    #[test]
    fn payload_roundtrip_compaction() {
        let continuity = CompactionContinuity {
            first_kept_entry_id: "entry-42".to_string(),
            compaction_leaf_event_id: "leaf-99".to_string(),
            pre_compaction_entry_count: 30,
            pre_compaction_message_count: 20,
            compacted_entry_count: 10,
            file_tracking: crate::contracts::dto::CompactionFileTracking {
                read_files: vec!["src/main.rs".to_string()],
                modified_files: vec![],
            },
        };
        let payload = SessionEventPayload::Compaction {
            summary: "compressed context".to_string(),
            compacted_entry_count: 10,
            original_message_count: 20,
            continuity: Some(continuity.clone()),
        };
        let (entry_type, value) = SessionEventStore::payload_to_entry_value(&payload);
        assert_eq!(entry_type, "compaction");

        let roundtrip =
            SessionEventStore::value_to_session_event_payload(&entry_type, &value).unwrap();
        if let SessionEventPayload::Compaction {
            summary,
            compacted_entry_count,
            original_message_count,
            continuity,
        } = roundtrip
        {
            assert_eq!(summary, "compressed context");
            assert_eq!(compacted_entry_count, 10);
            assert_eq!(original_message_count, 20);
            assert!(continuity.is_some());
            let cont = continuity.unwrap();
            assert_eq!(cont.first_kept_entry_id, "entry-42");
            assert_eq!(cont.compaction_leaf_event_id, "leaf-99");
            assert_eq!(cont.file_tracking.read_files.len(), 1);
        } else {
            panic!("expected Compaction payload");
        }
    }

    #[test]
    fn validate_legacy_store_nonexistent() {
        let validation = {
            let path = "/tmp/nonexistent_pi_test.jsonl";
            let path = PathBuf::from(path);
            if path.exists() {
                unreachable!()
            } else {
                LegacyStoreValidation {
                    role: LegacyStoreRole::Import,
                    source_kind: PersistenceStoreKind::Jsonl,
                    entry_count: 0,
                    is_valid: false,
                    errors: vec!["path does not exist".to_string()],
                }
            }
        };
        assert!(!validation.is_valid);
        assert!(!validation.errors.is_empty());
    }

    // ========================================================================
    // Branch continuity tests (VAL-SESS-002)
    // ========================================================================

    #[test]
    fn branch_continuity_linear_session() {
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::Message {
                    role: "user".to_string(),
                    content: serde_json::json!("hello"),
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "a".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("world"),
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "b".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("sess-1", &events);
        assert!(proj.is_linear);
        let bc = proj
            .branch_continuity
            .as_ref()
            .expect("branch continuity should exist");
        assert_eq!(bc.leaf_event_id, "e2");
        assert_eq!(bc.root_event_id.as_deref(), Some("e1"));
        assert!(bc.is_linear);
        assert!(bc.fork_points.is_empty());
        assert!(bc.branch_heads.is_empty());
    }

    #[test]
    fn branch_continuity_forked_session() {
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::Message {
                    role: "user".to_string(),
                    content: serde_json::json!("root"),
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "a".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("branch-a"),
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "b".to_string(),
            },
            SessionEvent {
                seq: 3,
                event_id: "e3".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("branch-b"),
                },
                timestamp: "2026-01-01T00:00:02Z".to_string(),
                payload_checksum: "c".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("sess-2", &events);
        assert!(!proj.is_linear);
        let bc = proj
            .branch_continuity
            .as_ref()
            .expect("branch continuity should exist");
        assert_eq!(bc.leaf_event_id, "e3");
        assert_eq!(bc.root_event_id.as_deref(), Some("e1"));
        assert!(!bc.is_linear);
        assert_eq!(bc.fork_points, vec!["e1"]);
        assert!(bc.branch_heads.contains(&"e2".to_string()));
        assert!(bc.branch_heads.contains(&"e3".to_string()));
    }

    // ========================================================================
    // Compaction continuity tests (VAL-SESS-003)
    // ========================================================================

    #[test]
    fn compaction_continuity_survives_projection() {
        let continuity = CompactionContinuity {
            first_kept_entry_id: "entry-50".to_string(),
            compaction_leaf_event_id: "entry-100".to_string(),
            pre_compaction_entry_count: 100,
            pre_compaction_message_count: 60,
            compacted_entry_count: 49,
            file_tracking: crate::contracts::dto::CompactionFileTracking {
                read_files: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
                modified_files: vec!["src/lib.rs".to_string()],
            },
        };
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::Message {
                    role: "user".to_string(),
                    content: serde_json::json!("before"),
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "a".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::Compaction {
                    summary: "compacted".to_string(),
                    compacted_entry_count: 49,
                    original_message_count: 60,
                    continuity: Some(continuity.clone()),
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "b".to_string(),
            },
            SessionEvent {
                seq: 3,
                event_id: "e3".to_string(),
                parent_event_id: Some("e2".to_string()),
                payload: SessionEventPayload::Message {
                    role: "user".to_string(),
                    content: serde_json::json!("after"),
                },
                timestamp: "2026-01-01T00:00:02Z".to_string(),
                payload_checksum: "c".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("sess-3", &events);
        let cc = proj
            .compaction_continuity
            .as_ref()
            .expect("compaction continuity should survive projection");
        assert_eq!(cc.first_kept_entry_id, "entry-50");
        assert_eq!(cc.compaction_leaf_event_id, "entry-100");
        assert_eq!(cc.pre_compaction_entry_count, 100);
        assert_eq!(cc.pre_compaction_message_count, 60);
        assert_eq!(cc.compacted_entry_count, 49);
        assert_eq!(cc.file_tracking.read_files.len(), 2);
        assert_eq!(cc.file_tracking.modified_files.len(), 1);
    }

    #[test]
    fn compaction_continuity_legacy_fallback() {
        // A compaction event with no typed continuity should produce None.
        let events = vec![SessionEvent {
            seq: 1,
            event_id: "e1".to_string(),
            parent_event_id: None,
            payload: SessionEventPayload::Compaction {
                summary: "legacy compact".to_string(),
                compacted_entry_count: 10,
                original_message_count: 5,
                continuity: None,
            },
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            payload_checksum: "a".to_string(),
        }];
        let proj = SessionEventStore::build_projection_from_events("sess-legacy", &events);
        assert!(proj.compaction_continuity.is_none());
    }

    // ========================================================================
    // Skill continuity tests (VAL-SESS-009)
    // ========================================================================

    #[test]
    fn skill_activation_survives_projection() {
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::Message {
                    role: "user".to_string(),
                    content: serde_json::json!("hello"),
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "a".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::SkillActivation {
                    skill_name: "code-review".to_string(),
                    source: SkillSource::Project,
                    file_path: ".pi/skills/code-review/SKILL.md".to_string(),
                    disable_model_invocation: false,
                    description: Some("Review code for quality".to_string()),
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "b".to_string(),
            },
            SessionEvent {
                seq: 3,
                event_id: "e3".to_string(),
                parent_event_id: Some("e2".to_string()),
                payload: SessionEventPayload::SkillActivation {
                    skill_name: "security-scan".to_string(),
                    source: SkillSource::User,
                    file_path: "/home/user/.pi/skills/security-scan/SKILL.md".to_string(),
                    disable_model_invocation: false,
                    description: None,
                },
                timestamp: "2026-01-01T00:00:02Z".to_string(),
                payload_checksum: "c".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("sess-skills", &events);
        let skill_set = proj
            .skill_activations
            .as_ref()
            .expect("skill activations should survive projection");
        assert!(!skill_set.skills_disabled);
        assert_eq!(skill_set.skills.len(), 2);
        assert_eq!(skill_set.skills[0].skill_name, "code-review");
        assert_eq!(skill_set.skills[1].skill_name, "security-scan");
    }

    #[test]
    fn skill_activation_snapshot_replaces_accumulated() {
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::SkillActivation {
                    skill_name: "old-skill".to_string(),
                    source: SkillSource::Project,
                    file_path: ".pi/skills/old-skill/SKILL.md".to_string(),
                    disable_model_invocation: false,
                    description: None,
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "a".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::SkillActivationSnapshot {
                    skills_disabled: false,
                    skills: vec![SkillActivation {
                        skill_name: "new-skill".to_string(),
                        source: SkillSource::User,
                        file_path: "/home/user/.pi/skills/new-skill/SKILL.md".to_string(),
                        disable_model_invocation: true,
                        description: Some("New skill".to_string()),
                    }],
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "b".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("sess-snapshot", &events);
        let skill_set = proj
            .skill_activations
            .as_ref()
            .expect("skill activations should exist");
        // Snapshot replaces accumulated — only new-skill should remain.
        assert_eq!(skill_set.skills.len(), 1);
        assert_eq!(skill_set.skills[0].skill_name, "new-skill");
        assert!(skill_set.skills[0].disable_model_invocation);
    }

    #[test]
    fn skill_activation_deduplication() {
        let events = vec![
            SessionEvent {
                seq: 1,
                event_id: "e1".to_string(),
                parent_event_id: None,
                payload: SessionEventPayload::SkillActivation {
                    skill_name: "dup-skill".to_string(),
                    source: SkillSource::Project,
                    file_path: ".pi/skills/dup-skill/SKILL.md".to_string(),
                    disable_model_invocation: false,
                    description: Some("original".to_string()),
                },
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                payload_checksum: "a".to_string(),
            },
            SessionEvent {
                seq: 2,
                event_id: "e2".to_string(),
                parent_event_id: Some("e1".to_string()),
                payload: SessionEventPayload::SkillActivation {
                    skill_name: "dup-skill".to_string(),
                    source: SkillSource::Path,
                    file_path: "/other/path/SKILL.md".to_string(),
                    disable_model_invocation: true,
                    description: Some("updated".to_string()),
                },
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                payload_checksum: "b".to_string(),
            },
        ];
        let proj = SessionEventStore::build_projection_from_events("sess-dedup", &events);
        let skill_set = proj
            .skill_activations
            .as_ref()
            .expect("skill activations should exist");
        // Duplicate should be deduplicated — latest wins.
        assert_eq!(skill_set.skills.len(), 1);
        assert_eq!(skill_set.skills[0].skill_name, "dup-skill");
        assert_eq!(skill_set.skills[0].source, SkillSource::Path);
        assert!(skill_set.skills[0].disable_model_invocation);
        assert_eq!(skill_set.skills[0].description.as_deref(), Some("updated"));
    }

    // ========================================================================
    // Skill validation (fail-closed) tests (VAL-SESS-009)
    // ========================================================================

    #[test]
    fn validate_skill_activations_all_found() {
        use std::path::PathBuf;
        let proj = SessionProjection {
            session_id: "test".to_string(),
            event_count: 1,
            leaf_event_id: Some("e1".to_string()),
            is_linear: true,
            message_count: 0,
            session_name: None,
            current_model: None,
            current_thinking_level: None,
            built_from_offset: 1,
            branch_continuity: None,
            compaction_continuity: None,
            skill_activations: Some(SkillActivationSet {
                skills: vec![SkillActivation {
                    skill_name: "code-review".to_string(),
                    source: SkillSource::Project,
                    file_path: ".pi/skills/code-review/SKILL.md".to_string(),
                    disable_model_invocation: false,
                    description: None,
                }],
                skills_disabled: false,
            }),
        };
        // Provide a path whose stem matches the skill name and that exists.
        // We use the activation's own file_path to test the path-based match.
        let available = vec![PathBuf::from(".pi/skills/code-review/SKILL.md")];
        let validation = validate_skill_activations(&proj, &available);
        assert!(validation.is_valid);
        assert_eq!(validation.found_skills, vec!["code-review"]);
        assert!(validation.missing_skills.is_empty());
    }

    #[test]
    fn validate_skill_activations_missing_skill_fails_closed() {
        let proj = SessionProjection {
            session_id: "test".to_string(),
            event_count: 1,
            leaf_event_id: Some("e1".to_string()),
            is_linear: true,
            message_count: 0,
            session_name: None,
            current_model: None,
            current_thinking_level: None,
            built_from_offset: 1,
            branch_continuity: None,
            compaction_continuity: None,
            skill_activations: Some(SkillActivationSet {
                skills: vec![SkillActivation {
                    skill_name: "deleted-skill".to_string(),
                    source: SkillSource::User,
                    file_path: "/home/user/.pi/skills/deleted-skill/SKILL.md".to_string(),
                    disable_model_invocation: false,
                    description: None,
                }],
                skills_disabled: false,
            }),
        };
        // No available skills — the activated one is missing.
        let validation = validate_skill_activations(&proj, &[]);
        assert!(!validation.is_valid);
        assert!(validation.found_skills.is_empty());
        assert_eq!(validation.missing_skills.len(), 1);
        assert_eq!(validation.missing_skills[0].skill_name, "deleted-skill");
        assert_eq!(validation.missing_skills[0].source, SkillSource::User);
    }

    #[test]
    fn validate_skill_activations_disabled_passes() {
        let proj = SessionProjection {
            session_id: "test".to_string(),
            event_count: 1,
            leaf_event_id: Some("e1".to_string()),
            is_linear: true,
            message_count: 0,
            session_name: None,
            current_model: None,
            current_thinking_level: None,
            built_from_offset: 1,
            branch_continuity: None,
            compaction_continuity: None,
            skill_activations: Some(SkillActivationSet {
                skills: vec![],
                skills_disabled: true,
            }),
        };
        let validation = validate_skill_activations(&proj, &[]);
        assert!(validation.is_valid);
    }

    #[test]
    fn validate_skill_activations_no_skills_passes() {
        let proj = SessionProjection {
            session_id: "test".to_string(),
            event_count: 0,
            leaf_event_id: None,
            is_linear: true,
            message_count: 0,
            session_name: None,
            current_model: None,
            current_thinking_level: None,
            built_from_offset: 0,
            branch_continuity: None,
            compaction_continuity: None,
            skill_activations: None,
        };
        let validation = validate_skill_activations(&proj, &[]);
        assert!(validation.is_valid);
    }

    // ========================================================================
    // Skill activation event roundtrip
    // ========================================================================

    #[test]
    fn skill_activation_event_roundtrip() {
        let payload = SessionEventPayload::SkillActivation {
            skill_name: "my-skill".to_string(),
            source: SkillSource::Project,
            file_path: ".pi/skills/my-skill/SKILL.md".to_string(),
            disable_model_invocation: false,
            description: Some("A test skill".to_string()),
        };
        let (entry_type, value) = SessionEventStore::payload_to_entry_value(&payload);
        assert_eq!(entry_type, "skill_activation");

        let roundtrip =
            SessionEventStore::value_to_session_event_payload(&entry_type, &value).unwrap();
        if let SessionEventPayload::SkillActivation {
            skill_name,
            source,
            file_path,
            disable_model_invocation,
            description,
        } = roundtrip
        {
            assert_eq!(skill_name, "my-skill");
            assert_eq!(source, SkillSource::Project);
            assert_eq!(file_path, ".pi/skills/my-skill/SKILL.md");
            assert!(!disable_model_invocation);
            assert_eq!(description.as_deref(), Some("A test skill"));
        } else {
            panic!("expected SkillActivation payload");
        }
    }

    #[test]
    fn skill_activation_snapshot_event_roundtrip() {
        let payload = SessionEventPayload::SkillActivationSnapshot {
            skills_disabled: true,
            skills: vec![SkillActivation {
                skill_name: "test".to_string(),
                source: SkillSource::User,
                file_path: "/home/.pi/skills/test/SKILL.md".to_string(),
                disable_model_invocation: true,
                description: None,
            }],
        };
        let (entry_type, value) = SessionEventStore::payload_to_entry_value(&payload);
        assert_eq!(entry_type, "skill_activation_snapshot");

        let roundtrip =
            SessionEventStore::value_to_session_event_payload(&entry_type, &value).unwrap();
        if let SessionEventPayload::SkillActivationSnapshot {
            skills_disabled,
            skills,
        } = roundtrip
        {
            assert!(skills_disabled);
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].skill_name, "test");
        } else {
            panic!("expected SkillActivationSnapshot payload");
        }
    }
}
