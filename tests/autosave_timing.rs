//! Autosave non-blocking timing integration tests.

use asupersync::runtime::RuntimeBuilder;
use pi::session::{AutosaveDurabilityMode, AutosaveFlushTrigger, Session};
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

#[test]
fn append_mutations_are_non_blocking_before_flush() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

    let start = Instant::now();
    for i in 0..2_000 {
        session.append_custom_entry(
            "autosave.test".to_string(),
            Some(serde_json::json!({ "seq": i, "kind": "timing" })),
        );
    }
    let elapsed = start.elapsed();

    let metrics = session.autosave_metrics();
    assert!(metrics.pending_mutations > 0);
    assert_eq!(metrics.flush_started, 0);
    assert!(
        elapsed < Duration::from_secs(2),
        "append path should stay non-blocking; elapsed={elapsed:?}"
    );
}

#[test]
fn periodic_flush_drains_queue_and_records_metrics() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

    for i in 0..256 {
        session.append_custom_entry(
            "autosave.test".to_string(),
            Some(serde_json::json!({ "seq": i })),
        );
    }

    let before = session.autosave_metrics();
    assert!(before.pending_mutations > 0);

    run_async(async {
        session
            .flush_autosave(AutosaveFlushTrigger::Periodic)
            .await
            .expect("periodic flush should succeed");
    });

    let after = session.autosave_metrics();
    assert_eq!(after.pending_mutations, 0);
    assert!(after.flush_started > before.flush_started);
    assert!(after.flush_succeeded > before.flush_succeeded);
    assert_eq!(
        after.last_flush_trigger,
        Some(AutosaveFlushTrigger::Periodic)
    );
}

#[test]
fn shutdown_flush_respects_durability_mode_semantics() {
    let temp = tempfile::tempdir().expect("tempdir");

    let mut strict = Session::create_with_dir(Some(temp.path().to_path_buf()));
    strict.set_autosave_durability_mode(AutosaveDurabilityMode::Strict);
    strict.append_custom_entry(
        "autosave.strict".to_string(),
        Some(serde_json::json!({ "x": 1 })),
    );

    run_async(async {
        strict
            .flush_autosave_on_shutdown()
            .await
            .expect("strict shutdown flush should succeed on writable path");
    });

    let strict_metrics = strict.autosave_metrics();
    assert_eq!(strict_metrics.pending_mutations, 0);
    assert!(strict_metrics.flush_started >= 1);

    let mut throughput = Session::create_with_dir(Some(temp.path().to_path_buf()));
    throughput.set_autosave_durability_mode(AutosaveDurabilityMode::Throughput);
    throughput.append_custom_entry(
        "autosave.throughput".to_string(),
        Some(serde_json::json!({ "x": 1 })),
    );

    let before = throughput.autosave_metrics();
    run_async(async {
        throughput
            .flush_autosave_on_shutdown()
            .await
            .expect("throughput mode shutdown should be a no-op");
    });
    let after = throughput.autosave_metrics();

    assert_eq!(after.flush_started, before.flush_started);
    assert!(after.pending_mutations > 0);
}
