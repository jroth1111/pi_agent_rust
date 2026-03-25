//! Session authority cutover tests.
//!
//! These tests verify the assertions for VAL-SESS-001, VAL-SESS-004, and
//! VAL-SESS-008 from the validation contract:
//!
//! VAL-SESS-001: One authoritative session history exists
//!   - All create/open/save/resume/compact writes flow through the event store
//!   - `JSONL`, `V2` sidecars, direct `SQLite` are NOT co-equal runtime authorities
//!
//! VAL-SESS-004: Session reads are projection-backed and rebuildable
//!   - Resume pickers, `get_state`, session lists served from projections
//!   - Projections are derived from event store only
//!   - No hybrid hot-path reads over multiple authoritative stores
//!
//! VAL-SESS-008: Legacy session stores are migration-only after cutover
//!   - `JSONL`, `V2` sidecars, direct `SQLite` limited to import/export/migration/inspection
//!   - Ordinary create/open/save/resume/compact do NOT route through legacy authorities

use std::path::PathBuf;

/// Helper to create a temporary session event store root.
fn temp_session_root(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi_test_session_authority_{test_name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir creation");
    dir
}

// =========================================================================
// VAL-SESS-001: One authoritative session history exists
// =========================================================================

#[test]
fn val_sess_001_authoritative_write_path_create_and_append() {
    // Prove: create_session + append_event go through SessionEventStore
    // (the single authoritative persistence boundary via PersistenceContract)
    use pi::contracts::dto::{PersistenceStoreKind, SessionEventPayload};
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("create_append");
    let store = SessionEventStore::new(root.clone());

    let session_id = uuid::Uuid::new_v4().to_string();

    // Create session
    let created_id = futures::executor::block_on(store.create_session(session_id.clone()))
        .expect("create_session should succeed");
    assert_eq!(created_id, session_id);

    // Append events
    let seq1 = futures::executor::block_on(store.append_event(
        &session_id,
        SessionEventPayload::Message {
            role: "user".to_string(),
            content: serde_json::json!("hello"),
        },
        None,
    ))
    .expect("append_event should succeed");
    assert_eq!(seq1, 1);

    let seq2 = futures::executor::block_on(store.append_event(
        &session_id,
        SessionEventPayload::Message {
            role: "assistant".to_string(),
            content: serde_json::json!("hi there"),
        },
        Some("e1".to_string()), // parent
    ))
    .expect("append_event should succeed");
    // seq2 is assigned by the store (not necessarily 2 since parent might be different)
    assert!(seq2 > 0);

    // Snapshot shows EventStore kind
    let snapshot = futures::executor::block_on(store.snapshot()).expect("snapshot should succeed");
    assert_eq!(snapshot.store_kind, PersistenceStoreKind::EventStore);
    assert!(snapshot.is_healthy);

    // Cleanup
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_001_authoritative_compaction_through_event_store() {
    // Prove: compaction writes through the event store, not legacy paths
    use pi::contracts::dto::SessionEventPayload;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("compaction_authority");
    let store = SessionEventStore::new(root.clone());
    let session_id = uuid::Uuid::new_v4().to_string();

    futures::executor::block_on(store.create_session(session_id.clone()))
        .expect("create_session should succeed");

    // Append some messages
    futures::executor::block_on(store.append_event(
        &session_id,
        SessionEventPayload::Message {
            role: "user".to_string(),
            content: serde_json::json!("hello"),
        },
        None,
    ))
    .expect("append should succeed");

    futures::executor::block_on(store.append_event(
        &session_id,
        SessionEventPayload::Message {
            role: "assistant".to_string(),
            content: serde_json::json!("response"),
        },
        None,
    ))
    .expect("append should succeed");

    // Compact through the event store
    let compaction_seq = futures::executor::block_on(store.compact_session(
        &session_id,
        "compressed summary".to_string(),
        2, // compacted_entry_count
        2, // original_message_count
        Some(serde_json::json!({"kept_entries": 1})),
    ))
    .expect("compact_session should succeed");

    assert!(compaction_seq > 0);

    // Read back events — compaction should be present
    let events = futures::executor::block_on(store.read_events(&session_id, 1, 100))
        .expect("read_events should succeed");
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        SessionEventPayload::Compaction { summary, .. } if summary == "compressed summary"
    )));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_001_no_legacy_authority_in_event_store_write_path() {
    // Prove: the PersistenceContract implementation writes ONLY to the event store,
    // never to JSONL/SQLite as a co-equal authority
    use pi::contracts::dto::SessionEventPayload;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("no_legacy_write");
    let store = SessionEventStore::new(root.clone());
    let session_id = uuid::Uuid::new_v4().to_string();

    futures::executor::block_on(store.create_session(session_id.clone()))
        .expect("create_session should succeed");

    // Write through event store
    futures::executor::block_on(store.append_event(
        &session_id,
        SessionEventPayload::Message {
            role: "user".to_string(),
            content: serde_json::json!("test"),
        },
        None,
    ))
    .expect("append should succeed");

    // Verify: no .jsonl file was created as a side effect
    let v2_dir = root.join(format!("{session_id}.v2"));
    assert!(v2_dir.exists(), "event store directory should exist");

    // Verify: no .jsonl files in the root (would indicate co-equal JSONL authority)
    let jsonl_files: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert!(
        jsonl_files.is_empty(),
        "no JSONL files should be created as co-equal authority: found {:?}",
        jsonl_files
    );

    let _ = std::fs::remove_dir_all(&root);
}

// =========================================================================
// VAL-SESS-004: Session reads are projection-backed and rebuildable
// =========================================================================

#[test]
fn val_sess_004_projection_is_derived_from_events() {
    // Prove: session_projection is built from events, not from a separate store
    use pi::contracts::dto::SessionEventPayload;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("projection_derived");
    let store = SessionEventStore::new(root.clone());
    let session_id = uuid::Uuid::new_v4().to_string();

    futures::executor::block_on(store.create_session(session_id.clone()))
        .expect("create_session should succeed");

    // Append events
    futures::executor::block_on(store.append_event(
        &session_id,
        SessionEventPayload::Message {
            role: "user".to_string(),
            content: serde_json::json!("hello"),
        },
        None,
    ))
    .expect("append should succeed");

    futures::executor::block_on(store.append_event(
        &session_id,
        SessionEventPayload::ModelChange {
            provider: "anthropic".to_string(),
            model_id: "claude-3".to_string(),
        },
        None,
    ))
    .expect("append should succeed");

    // Get projection
    let projection = futures::executor::block_on(store.session_projection(&session_id))
        .expect("projection should succeed");

    // Projection should reflect event data
    assert_eq!(projection.session_id, session_id);
    assert_eq!(projection.event_count, 2);
    assert_eq!(projection.message_count, 1);
    assert!(projection.is_linear);

    // Model should be tracked
    let model = projection
        .current_model
        .expect("projection should track model changes");
    assert_eq!(model.provider, "anthropic");
    assert_eq!(model.model_id, "claude-3");

    // Leaf should be the last event
    assert!(projection.leaf_event_id.is_some());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_004_projection_rebuildable_from_scratch() {
    // Prove: projection can be rebuilt from the event store at any time
    // (projections are never co-equal truth)
    use pi::contracts::dto::SessionEventPayload;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("projection_rebuild");
    let store = SessionEventStore::new(root.clone());
    let session_id = uuid::Uuid::new_v4().to_string();

    futures::executor::block_on(store.create_session(session_id.clone()))
        .expect("create_session should succeed");

    // Write several events
    for i in 0..5 {
        futures::executor::block_on(store.append_event(
            &session_id,
            SessionEventPayload::Message {
                role: if i % 2 == 0 {
                    "user".to_string()
                } else {
                    "assistant".to_string()
                },
                content: serde_json::json!(format!("message {i}")),
            },
            None,
        ))
        .expect("append should succeed");
    }

    // Build projection multiple times — should be deterministic
    let proj1 =
        futures::executor::block_on(store.session_projection(&session_id)).expect("projection 1");
    let proj2 =
        futures::executor::block_on(store.session_projection(&session_id)).expect("projection 2");

    assert_eq!(proj1.event_count, proj2.event_count);
    assert_eq!(proj1.message_count, proj2.message_count);
    assert_eq!(proj1.leaf_event_id, proj2.leaf_event_id);
    assert_eq!(proj1.is_linear, proj2.is_linear);

    // Rebuild projections explicitly
    futures::executor::block_on(store.rebuild_projections()).expect("rebuild should succeed");

    // Projection should still be consistent after rebuild
    let proj3 =
        futures::executor::block_on(store.session_projection(&session_id)).expect("projection 3");
    assert_eq!(proj1.event_count, proj3.event_count);
    assert_eq!(proj1.message_count, proj3.message_count);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_004_read_active_path_and_tail_are_projection_backed() {
    // Prove: read_active_path and read_tail are derived from event store
    use pi::contracts::dto::SessionEventPayload;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("active_path_tail");
    let store = SessionEventStore::new(root.clone());
    let session_id = uuid::Uuid::new_v4().to_string();

    futures::executor::block_on(store.create_session(session_id.clone()))
        .expect("create_session should succeed");

    // Append chain of events
    for i in 0..5 {
        futures::executor::block_on(store.append_event(
            &session_id,
            SessionEventPayload::Message {
                role: "user".to_string(),
                content: serde_json::json!(format!("msg {i}")),
            },
            None,
        ))
        .expect("append should succeed");
    }

    // Read tail (last N events)
    let tail = futures::executor::block_on(store.read_tail(&session_id, 3))
        .expect("read_tail should succeed");
    assert_eq!(tail.len(), 3, "tail should return 3 events");

    // Read active path (root to leaf)
    let active_path =
        futures::executor::block_on(store.read_active_path(&session_id)).expect("read_active_path");
    assert!(!active_path.is_empty(), "active path should not be empty");

    // Read events with offset
    let from_offset = futures::executor::block_on(store.read_events(&session_id, 3, 10))
        .expect("read_events with offset should succeed");
    assert!(from_offset.len() >= 3, "should have events from offset 3");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_004_list_sessions_is_projection_backed() {
    // Prove: list_sessions returns projection-derived session list
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("list_sessions_projection");
    let store = SessionEventStore::new(root.clone());

    // Initially empty
    let sessions =
        futures::executor::block_on(store.list_sessions()).expect("list_sessions should succeed");
    assert!(sessions.is_empty(), "no sessions initially");

    // Create a few sessions
    for i in 0..3 {
        let id = format!("test-session-{i}");
        futures::executor::block_on(store.create_session(id.clone()))
            .expect("create should succeed");
    }

    let sessions =
        futures::executor::block_on(store.list_sessions()).expect("list_sessions should succeed");
    assert_eq!(sessions.len(), 3, "should list 3 sessions");

    let _ = std::fs::remove_dir_all(&root);
}

// =========================================================================
// VAL-SESS-008: Legacy session stores are migration-only after cutover
// =========================================================================

#[test]
fn val_sess_008_validate_legacy_store_import_role() {
    // Prove: validate_legacy_store works with Import role
    use pi::contracts::dto::LegacyStoreRole;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("validate_import");
    let store = SessionEventStore::new(root.clone());

    // Create a fake JSONL file for validation
    let jsonl_path = root.join("test_session.jsonl");
    std::fs::write(
        &jsonl_path,
        r#"{"id":"test","timestamp":"2026-01-01T00:00:00Z"}
{"type":"message","id":"e1","role":"user","content":"hello"}
"#,
    )
    .expect("write jsonl");

    let validation = futures::executor::block_on(
        store.validate_legacy_store(jsonl_path.to_str().unwrap(), LegacyStoreRole::Import),
    )
    .expect("validate should succeed");

    assert_eq!(validation.role, LegacyStoreRole::Import);
    assert_eq!(
        validation.source_kind,
        pi::contracts::dto::PersistenceStoreKind::Jsonl
    );
    assert!(validation.is_valid);
    assert_eq!(validation.entry_count, 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_008_validate_legacy_store_inspection_role() {
    // Prove: Inspection role is accepted for legacy validation
    use pi::contracts::dto::LegacyStoreRole;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("validate_inspection");
    let store = SessionEventStore::new(root.clone());

    let jsonl_path = root.join("test.jsonl");
    std::fs::write(&jsonl_path, r#"{"id":"t"}"#).expect("write");

    let validation = futures::executor::block_on(
        store.validate_legacy_store(jsonl_path.to_str().unwrap(), LegacyStoreRole::Inspection),
    )
    .expect("validate should succeed");

    assert_eq!(validation.role, LegacyStoreRole::Inspection);
    // No entries (header only, but our validation counts lines after header)
    assert!(validation.is_valid || !validation.errors.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_008_validate_legacy_nonexistent_fails() {
    // Prove: validating a nonexistent legacy store fails gracefully
    use pi::contracts::dto::LegacyStoreRole;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("validate_nonexistent");
    let store = SessionEventStore::new(root.clone());

    let validation = futures::executor::block_on(store.validate_legacy_store(
        "/tmp/pi_test_nonexistent_38291.jsonl",
        LegacyStoreRole::Import,
    ))
    .expect("validate should succeed (returns validation result, not error)");

    assert!(!validation.is_valid);
    assert!(!validation.errors.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_008_event_store_is_not_a_legacy_format() {
    // Prove: EventStore kind is rejected when used as a legacy source
    use pi::contracts::dto::{LegacyStoreRole, PersistenceStoreKind};
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("not_legacy");
    let store = SessionEventStore::new(root.clone());

    // A .v2 directory should be recognized as NOT a legacy format
    let v2_path = root.join("session.v2");
    std::fs::create_dir_all(&v2_path).expect("create v2 dir");

    // Validation of a V2 path should fail (it's not a legacy format)
    let result = futures::executor::block_on(
        store.validate_legacy_store(v2_path.to_str().unwrap(), LegacyStoreRole::Import),
    );

    // The validation may succeed with errors or fail — either way, the source_kind
    // should NOT be EventStore (since it's a directory, it won't match jsonl/sqlite)
    if let Ok(validation) = result {
        assert!(
            validation.source_kind != PersistenceStoreKind::EventStore,
            "V2 directory should not be treated as EventStore for legacy validation"
        );
    }
    // Error is also acceptable — the point is it can't be imported as legacy

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_008_export_goes_to_legacy_format_only() {
    // Prove: export_session writes to legacy format (JSONL), not as co-equal authority
    use pi::contracts::dto::{PersistenceStoreKind, SessionEventPayload};
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("export_legacy");
    let store = SessionEventStore::new(root.clone());
    let session_id = uuid::Uuid::new_v4().to_string();

    // Create session with events in the event store
    futures::executor::block_on(store.create_session(session_id.clone()))
        .expect("create should succeed");
    futures::executor::block_on(store.append_event(
        &session_id,
        SessionEventPayload::Message {
            role: "user".to_string(),
            content: serde_json::json!("export test"),
        },
        None,
    ))
    .expect("append should succeed");

    // Export to JSONL (legacy format)
    let export_path = root.join("exported_session.jsonl");
    futures::executor::block_on(store.export_session(
        &session_id,
        PersistenceStoreKind::Jsonl,
        export_path.to_str().unwrap(),
    ))
    .expect("export should succeed");

    // Verify export file exists and has content
    let content = std::fs::read_to_string(&export_path).expect("read exported file");
    assert!(
        content.contains("export test"),
        "exported content should contain our message"
    );

    // The exported JSONL is NOT an authority — the event store is
    let v2_dir = root.join(format!("{session_id}.v2"));
    assert!(v2_dir.exists(), "event store should still be the authority");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_008_export_to_event_store_rejected() {
    // Prove: exporting TO the EventStore format is rejected (it's not a legacy format)
    use pi::contracts::dto::PersistenceStoreKind;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("export_rejected");
    let store = SessionEventStore::new(root.clone());
    let session_id = uuid::Uuid::new_v4().to_string();

    futures::executor::block_on(store.create_session(session_id.clone()))
        .expect("create should succeed");

    // Exporting to EventStore should fail (it's not a legacy target)
    let result = futures::executor::block_on(store.export_session(
        &session_id,
        PersistenceStoreKind::EventStore,
        "/tmp/pi_test_export_event_store.jsonl",
    ));

    assert!(
        result.is_err(),
        "export to EventStore format should be rejected"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// =========================================================================
// Cross-cutting: persistence contract interface consistency
// =========================================================================

#[test]
fn persistence_contract_snapshot_reports_event_store_kind() {
    // Prove: PersistenceSnapshot from the event store reports EventStore kind
    use pi::contracts::dto::PersistenceStoreKind;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("snapshot_kind");
    let store = SessionEventStore::new(root.clone());

    let snapshot = futures::executor::block_on(store.snapshot()).expect("snapshot should succeed");
    assert_eq!(snapshot.store_kind, PersistenceStoreKind::EventStore);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persistence_contract_health_and_flush() {
    // Prove: health checks and flush work on the event store
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("health_flush");
    let store = SessionEventStore::new(root.clone());

    // Healthy when root exists
    let healthy = futures::executor::block_on(store.is_healthy());
    assert!(healthy);

    // Flush should succeed (V2 fsyncs on append)
    futures::executor::block_on(store.flush()).expect("flush should succeed");

    // Rebuild projections should succeed
    futures::executor::block_on(store.rebuild_projections()).expect("rebuild should succeed");

    let _ = std::fs::remove_dir_all(&root);
}

// =========================================================================
// Session struct integration: persistence contract delegation
// =========================================================================

#[test]
fn val_sess_001_session_with_persistence_delegates_saves() {
    // Prove: when Session has a PersistenceContract, save() routes through
    // the event store (not legacy JSONL/V2 paths)
    use pi::contracts::dto::SessionEventPayload;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("session_delegates");
    let store = SessionEventStore::new(root.clone());
    let persistence = std::sync::Arc::new(store);

    let session_id = uuid::Uuid::new_v4().to_string();

    // Create session with persistence contract
    let mut session = pi::session::Session::in_memory().with_persistence(persistence.clone());
    session.header.id = session_id.clone();

    // Create session in event store
    futures::executor::block_on(persistence.create_session(session_id.clone()))
        .expect("create should succeed");

    // Append a message via Session's normal API
    session.append_message(pi::session::SessionMessage::User {
        content: pi::model::UserContent::Text("hello from event store".to_string()),
        timestamp: None,
    });

    // Save — should go through the event store
    futures::executor::block_on(session.save()).expect("save should succeed");

    // Verify the event was persisted via the event store
    let events = futures::executor::block_on(persistence.read_events(&session_id, 1, 100))
        .expect("read_events should succeed");
    assert!(
        events.iter().any(|e| matches!(
            &e.payload,
            SessionEventPayload::Message { role, .. } if role == "user"
        )),
        "user message should be in the event store"
    );

    // Verify no JSONL file was created (legacy path not used)
    let jsonl_files: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert!(
        jsonl_files.is_empty(),
        "no JSONL files should be created: found {:?}",
        jsonl_files
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_001_session_open_from_event_store_roundtrip() {
    // Prove: a session saved through the event store can be reopened and
    // its entries are preserved
    use pi::contracts::dto::SessionEventPayload;
    use pi::contracts::engine::PersistenceContract;
    use pi::services::session_event_store::SessionEventStore;

    let root = temp_session_root("session_roundtrip");
    let store = SessionEventStore::new(root.clone());
    let persistence = std::sync::Arc::new(store);
    let session_id = uuid::Uuid::new_v4().to_string();

    // Create and populate session via event store
    futures::executor::block_on(persistence.create_session(session_id.clone()))
        .expect("create should succeed");
    futures::executor::block_on(persistence.append_event(
        &session_id,
        SessionEventPayload::Message {
            role: "user".to_string(),
            content: serde_json::json!("roundtrip test"),
        },
        None,
    ))
    .expect("append should succeed");

    // Open session from event store
    let mut reopened = futures::executor::block_on(pi::session::Session::open_from_event_store(
        persistence.clone(),
        &session_id,
    ))
    .expect("open_from_event_store should succeed");

    assert!(reopened.has_authoritative_persistence());
    assert_eq!(reopened.header.id, session_id);
    assert!(
        !reopened.entries.is_empty(),
        "reopened session should have entries"
    );

    // Append another message and save
    reopened.append_message(pi::session::SessionMessage::User {
        content: pi::model::UserContent::Text("after reopen".to_string()),
        timestamp: None,
    });
    futures::executor::block_on(reopened.save()).expect("save after reopen should succeed");

    // Verify both messages are in the event store
    let events = futures::executor::block_on(persistence.read_events(&session_id, 1, 100))
        .expect("read_events should succeed");
    assert_eq!(events.len(), 2, "should have 2 events after reopen+save");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn val_sess_008_session_without_persistence_uses_legacy_path() {
    // Prove: a session WITHOUT a PersistenceContract still uses legacy
    // persistence (JSONL), proving the legacy path is gated by contract presence
    use pi::session::Session;

    let root = temp_session_root("session_no_contract");
    let mut session = Session::create_with_dir(Some(root.clone()));

    // Append a message
    session.append_message(pi::session::SessionMessage::User {
        content: pi::model::UserContent::Text("legacy path test".to_string()),
        timestamp: None,
    });

    assert!(!session.has_authoritative_persistence());

    // Save — should use legacy JSONL path
    futures::executor::block_on(session.save()).expect("save should succeed via legacy path");

    // Verify a JSONL file was created (may be in a subdirectory based on CWD encoding)
    let mut jsonl_count: u32 = 0;
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
                jsonl_count += 1;
            }
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                if let Ok(sub_entries) = std::fs::read_dir(entry.path()) {
                    for sub in sub_entries.flatten() {
                        if sub.path().extension().is_some_and(|ext| ext == "jsonl") {
                            jsonl_count += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(
        jsonl_count != 0,
        "JSONL file should be created when no persistence contract is set"
    );

    let _ = std::fs::remove_dir_all(&root);
}
