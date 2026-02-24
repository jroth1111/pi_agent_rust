//! Large-session scale and latency integration tests.

use asupersync::runtime::RuntimeBuilder;
use pi::model::UserContent;
use pi::session::{Session, SessionEntry, SessionMessage};
use std::time::{Duration, Instant};

fn run_async<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    runtime.block_on(future)
}

const fn user_message(text: String) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text),
        timestamp: Some(0),
    }
}

fn build_large_session(entry_count: usize, payload_bytes: usize) -> (Session, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

    let payload = "x".repeat(payload_bytes.max(16));
    for i in 0..entry_count {
        let text = format!("message-{i}-{payload}");
        session.append_message(user_message(text));
    }

    run_async(async { session.save().await.expect("save large session") });
    (session, temp)
}

#[test]
fn large_session_load_latency_stays_bounded() {
    let (session, _temp) = build_large_session(4_000, 256);
    let path = session.path.expect("session path");

    let start = Instant::now();
    let (_loaded, diagnostics) = run_async(async {
        Session::open_with_diagnostics(path.to_string_lossy().as_ref())
            .await
            .expect("open with diagnostics")
    });
    let elapsed = start.elapsed();

    assert!(diagnostics.skipped_entries.is_empty());
    assert!(
        elapsed < Duration::from_secs(6),
        "large session load exceeded budget: {elapsed:?}"
    );
}

#[test]
fn large_session_branch_navigation_latency_stays_bounded() {
    let (mut session, _temp) = build_large_session(3_000, 192);
    let target_id = session.entries[1_500].base_id().cloned().expect("entry id");

    let start = Instant::now();
    let ok = session.navigate_to(&target_id);
    let elapsed = start.elapsed();

    assert!(ok, "navigate_to should find existing entry");
    assert!(
        elapsed < Duration::from_millis(200),
        "branch navigation exceeded budget: {elapsed:?}"
    );
}

#[test]
fn search_like_scan_over_large_session_stays_bounded() {
    let (session, _temp) = build_large_session(5_000, 128);
    let start = Instant::now();

    let mut matches = 0usize;
    for entry in session.entries_for_current_path() {
        if let SessionEntry::Message(msg) = entry {
            if let SessionMessage::User { content, .. } = &msg.message {
                if let UserContent::Text(text) = content {
                    if text.contains("message-49") {
                        matches += 1;
                    }
                }
            }
        }
    }

    let elapsed = start.elapsed();
    assert!(matches > 0, "expected search corpus match");
    assert!(
        elapsed < Duration::from_millis(800),
        "search-like scan exceeded budget: {elapsed:?}"
    );
}
