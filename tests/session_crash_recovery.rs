//! Focused crash-recovery integration tests for session persistence.
//!
//! This target exists to provide a stable test entrypoint for
//! `cargo test --test session_crash_recovery`.

use asupersync::runtime::RuntimeBuilder;
use pi::model::UserContent;
use pi::session::{Session, SessionMessage};
use serde_json::json;
use std::future::Future;
use std::io::Write as _;

fn run_async<T>(future: impl Future<Output = T>) -> T {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(future)
}

fn make_test_message(text: &str) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text.to_string()),
        timestamp: Some(0),
    }
}

fn valid_header_json() -> String {
    serde_json::to_string(&json!({
        "type": "session",
        "version": 3,
        "id": "session-crash-recovery",
        "timestamp": "2024-06-01T00:00:00.000Z",
        "cwd": "/tmp/test"
    }))
    .expect("serialize header")
}

fn valid_entry_json(id: &str, text: &str) -> String {
    json!({
        "type": "message",
        "id": id,
        "timestamp": "2024-06-01T00:00:00.000Z",
        "message": {"role": "user", "content": text}
    })
    .to_string()
}

#[test]
fn truncated_last_entry_recovers_preceding_entries() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("truncated.jsonl");

    let content = format!(
        "{}\n{}\n{}\n{{\"type\":\"message\",\"id\":\"partial",
        valid_header_json(),
        valid_entry_json("ok-1", "first"),
        valid_entry_json("ok-2", "second")
    );
    std::fs::write(&file_path, content).expect("write test file");

    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .expect("recover truncated file");

    assert_eq!(session.entries.len(), 2, "valid entries should survive");
    assert_eq!(
        diagnostics.skipped_entries.len(),
        1,
        "truncated line skipped"
    );
}

#[test]
fn corrupted_jsonl_entry_is_skipped() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("corrupted.jsonl");

    let content = format!(
        "{}\n{}\nNOT_VALID_JSON\n{}\n",
        valid_header_json(),
        valid_entry_json("ok-1", "before"),
        valid_entry_json("ok-2", "after"),
    );
    std::fs::write(&file_path, content).expect("write test file");

    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .expect("recover file with bad line");

    assert_eq!(
        session.entries.len(),
        2,
        "good entries around corruption survive"
    );
    assert_eq!(
        diagnostics.skipped_entries.len(),
        1,
        "only corrupted line skipped"
    );
}

#[test]
fn incremental_append_partial_write_recovers_cleanly() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    session.append_message(make_test_message("first"));
    session.append_message(make_test_message("second"));
    run_async(async { session.save().await }).expect("initial save");
    let path = session.path.clone().expect("session path");

    // Simulate crash mid-append by writing a partial JSON payload.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open session file");
    write!(file, "{{\"type\":\"message\",\"id\":\"partial").expect("append partial");
    drop(file);

    let (loaded, diagnostics) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .expect("recover partial append");

    assert_eq!(loaded.entries.len(), 2, "original entries preserved");
    assert_eq!(diagnostics.skipped_entries.len(), 1, "partial line skipped");
}

#[test]
fn recover_then_continue_writes_successfully() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("recover_then_continue.jsonl");

    let content = format!(
        "{}\n{}\n{{BROKEN}}\n",
        valid_header_json(),
        valid_entry_json("ok-1", "before"),
    );
    std::fs::write(&file_path, content).expect("write corrupted seed file");

    let (mut session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .expect("recover file");
    assert_eq!(session.entries.len(), 1);
    assert_eq!(diagnostics.skipped_entries.len(), 1);

    session.append_message(make_test_message("after-recovery"));
    run_async(async { session.save().await }).expect("save after recovery");

    let reloaded = run_async(async { Session::open(file_path.to_string_lossy().as_ref()).await })
        .expect("reopen after continued write");
    assert_eq!(
        reloaded.entries.len(),
        2,
        "new write persisted after recovery"
    );
}
