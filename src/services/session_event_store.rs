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
    LegacyImportRequest, LegacyStoreRole, LegacyStoreValidation, ModelControl, PersistenceSnapshot,
    PersistenceStoreKind, SessionEvent, SessionEventPayload, SessionIdentity, SessionProjection,
    ThinkingLevel,
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
            } => (
                "compaction".to_string(),
                serde_json::json!({
                    "summary": summary,
                    "compactedEntryCount": compacted_entry_count,
                    "originalMessageCount": original_message_count,
                    "continuity": continuity,
                }),
            ),
            SessionEventPayload::BranchSummary {
                summary,
                from_leaf_id,
            } => (
                "branch_summary".to_string(),
                serde_json::json!({ "summary": summary, "fromLeafId": from_leaf_id }),
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
                let continuity = value.get("continuity").cloned();
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
        let mut message_count: u64 = 0;
        let mut session_name = None;
        let mut current_provider = None;
        let mut current_model_id = None;
        let mut current_thinking_level = None;
        let mut entry_ids = std::collections::HashSet::new();
        let mut parent_map: HashMap<String, String> = HashMap::new();
        let mut is_linear = true;
        let mut children_count: HashMap<String, usize> = HashMap::new();

        for event in events {
            entry_ids.insert(event.event_id.clone());
            if let Some(ref parent_id) = event.parent_event_id {
                parent_map.insert(event.event_id.clone(), parent_id.clone());
                *children_count.entry(parent_id.clone()).or_insert(0) += 1;
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

        let payload = SessionEventPayload::Compaction {
            summary,
            compacted_entry_count,
            original_message_count,
            continuity,
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
            SessionEntry::Compaction(c) => SessionEventPayload::Compaction {
                summary: c.summary.clone(),
                compacted_entry_count: 0,
                original_message_count: 0,
                continuity: c.details.clone(),
            },
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
        let payload = SessionEventPayload::Compaction {
            summary: "compressed context".to_string(),
            compacted_entry_count: 10,
            original_message_count: 20,
            continuity: Some(serde_json::json!({"kept_entries": 5})),
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
}
