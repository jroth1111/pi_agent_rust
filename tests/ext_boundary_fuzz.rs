//! Extension JS boundary fuzz/property tests.
//!
//! Focus areas:
//! - Hostcall request boundary canonicalization and hashing
//! - UTF-8 edge cases for extension entry paths
//! - Malformed extension source loading resilience
//! - `QuickJS` memory-limit enforcement monotonicity
//! - Extension event serialization/deserialization robustness

#![forbid(unsafe_code)]

mod common;

use pi::extension_events::{ExtensionEvent, InputEventResult, ToolResultEventResult};
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::{
    HostcallKind, HostcallRequest, PiJsRuntime, PiJsRuntimeConfig, PiJsRuntimeLimits,
};
use pi::scheduler::DeterministicClock;
use pi::tools::ToolRegistry;
use proptest::prelude::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn weird_string() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            (0x20u32..=0x7eu32).prop_map(|cp| char::from_u32(cp).expect("ascii")),
            (0x80u32..=0xd7ffu32).prop_map(|cp| char::from_u32(cp).expect("bmp")),
            (0xe000u32..=0x10ffffu32).prop_map(|cp| char::from_u32(cp).expect("unicode")),
            Just('\0'),
        ],
        0..64,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn safe_utf8_component() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            (b'a'..=b'z').prop_map(char::from),
            (b'A'..=b'Z').prop_map(char::from),
            (b'0'..=b'9').prop_map(char::from),
            Just('_'),
            Just('-'),
            Just('é'),
            Just('ß'),
            Just('中'),
            Just('Ж'),
            Just('🙂'),
        ],
        1..24,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn tool_name_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("read".to_string()),
        Just("write".to_string()),
        Just("grep".to_string()),
        Just("find".to_string()),
        Just("ls".to_string()),
        Just("edit".to_string()),
        Just("bash".to_string()),
        weird_string(),
    ]
}

fn op_name_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("get_state".to_string()),
        Just("set_state".to_string()),
        weird_string()
    ]
}

fn json_leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        weird_string().prop_map(Value::String),
    ]
}

fn json_value() -> impl Strategy<Value = Value> {
    json_leaf().prop_recursive(3, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map("[A-Za-z0-9_]{1,12}", inner, 0..6).prop_map(|map| {
                let mut out = serde_json::Map::new();
                for (k, v) in map {
                    out.insert(k, v);
                }
                Value::Object(out)
            }),
        ]
    })
}

fn hostcall_kind_with_payload() -> impl Strategy<Value = (HostcallKind, Value)> {
    prop_oneof![
        (tool_name_strategy(), json_value())
            .prop_map(|(name, payload)| (HostcallKind::Tool { name }, payload)),
        (weird_string(), json_value())
            .prop_map(|(cmd, payload)| (HostcallKind::Exec { cmd }, payload)),
        json_value().prop_map(|payload| (HostcallKind::Http, payload)),
        (op_name_strategy(), json_value())
            .prop_map(|(op, payload)| (HostcallKind::Session { op }, payload)),
        (op_name_strategy(), json_value())
            .prop_map(|(op, payload)| (HostcallKind::Ui { op }, payload)),
        (op_name_strategy(), json_value())
            .prop_map(|(op, payload)| (HostcallKind::Events { op }, payload)),
        json_value().prop_map(|payload| (HostcallKind::Log, payload)),
    ]
}

fn request(kind: HostcallKind, payload: Value) -> HostcallRequest {
    HostcallRequest {
        call_id: "call-boundary".to_string(),
        kind,
        payload,
        trace_id: 0,
        extension_id: Some("ext-boundary".to_string()),
    }
}

fn load_extension_source(source: &str, limits: PiJsRuntimeLimits) -> Result<(), String> {
    let harness = common::TestHarness::new("ext_boundary_fuzz");
    let cwd = harness.temp_dir().to_path_buf();
    let entry = harness.create_file("extensions/boundary/index.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&entry).map_err(|e| e.to_string())?;

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        limits,
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(config, tools, manager).await }
    })
    .map_err(|e| e.to_string())?;
    manager.set_js_runtime(runtime);

    let load_result = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    })
    .map_err(|e| e.to_string());

    common::run_async({
        let manager = manager;
        async move {
            let _ = manager.shutdown(Duration::from_secs(3)).await;
        }
    });

    load_result
}

fn eval_with_memory_limit(limit_bytes: usize, rounds: usize, width: usize) -> Result<(), String> {
    futures::executor::block_on(async move {
        let config = PiJsRuntimeConfig {
            limits: PiJsRuntimeLimits {
                memory_limit_bytes: Some(limit_bytes),
                ..Default::default()
            },
            ..Default::default()
        };

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .map_err(|e| e.to_string())?;

        let script = format!(
            "const chunks = []; for (let i = 0; i < {rounds}; i++) {{ chunks.push('x'.repeat({width})); }}"
        );

        runtime.eval(&script).await.map_err(|e| e.to_string())
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 768,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn hostcall_request_boundary_is_total((kind, payload) in hostcall_kind_with_payload()) {
        let req = request(kind, payload);

        let method = req.method();
        prop_assert!(matches!(method, "tool" | "exec" | "http" | "session" | "ui" | "events" | "log"));

        let capability = req.required_capability();
        prop_assert!(matches!(
            capability,
            "read" | "write" | "exec" | "tool" | "http" | "session" | "ui" | "events" | "log"
        ));

        let params = req.params_for_hash();
        let hash = req.params_hash();
        prop_assert_eq!(hash.len(), 64);
        prop_assert!(hash.chars().all(|ch| matches!(ch, '0'..='9' | 'a'..='f')));
        prop_assert_eq!(hash, req.params_hash());

        match req.kind {
            HostcallKind::Tool { ref name } => {
                prop_assert_eq!(params.get("name"), Some(&Value::String(name.clone())));
                prop_assert!(params.get("input").is_some());
            }
            HostcallKind::Exec { ref cmd } => {
                prop_assert_eq!(params.get("cmd"), Some(&Value::String(cmd.clone())));
            }
            HostcallKind::Session { ref op } | HostcallKind::Ui { ref op } | HostcallKind::Events { ref op } => {
                prop_assert_eq!(params.get("op"), Some(&Value::String(op.clone())));
            }
            HostcallKind::Http | HostcallKind::Log => {
                prop_assert_eq!(params, req.payload);
            }
        }
    }

    #[test]
    fn op_hostcalls_wrap_non_object_payloads(op in op_name_strategy(), payload in json_leaf().prop_filter("non-object", |v| !v.is_object() && !v.is_null())) {
        let session = request(HostcallKind::Session { op: op.clone() }, payload.clone());
        let params = session.params_for_hash();
        prop_assert_eq!(params.get("op"), Some(&Value::String(op.clone())));
        prop_assert_eq!(params.get("payload"), Some(&payload));

        let ui = request(HostcallKind::Ui { op: op.clone() }, payload.clone());
        let ui_params = ui.params_for_hash();
        prop_assert_eq!(ui_params.get("op"), Some(&Value::String(op.clone())));
        prop_assert_eq!(ui_params.get("payload"), Some(&payload));

        let events = request(HostcallKind::Events { op }, payload.clone());
        let events_params = events.params_for_hash();
        prop_assert_eq!(events_params.get("payload"), Some(&payload));
    }

    #[test]
    fn exec_kind_cmd_overrides_payload_cmd(
        cmd_kind in weird_string(),
        cmd_payload in weird_string(),
        extras in prop::collection::btree_map(
            "[A-Za-z0-9_]{1,10}".prop_filter("key != cmd", |k| k != "cmd"),
            json_leaf(),
            0..4,
        ),
    ) {
        let mut obj = serde_json::Map::new();
        obj.insert("cmd".to_string(), Value::String(cmd_payload));
        for (k, v) in extras {
            obj.insert(k, v);
        }

        let req = request(HostcallKind::Exec { cmd: cmd_kind.clone() }, Value::Object(obj));
        let params = req.params_for_hash();
        prop_assert_eq!(params.get("cmd"), Some(&Value::String(cmd_kind)));
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn utf8_entry_paths_always_produce_non_empty_ids(component in safe_utf8_component()) {
        let harness = common::TestHarness::new("ext_boundary_utf8_path");
        let rel = format!("extensions/{component}.mjs");
        let entry = harness.create_file(&rel, b"export default function activate(_pi) {}");

        let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("spec from utf8 entry path");
        prop_assert!(!spec.extension_id.trim().is_empty());
        prop_assert_eq!(spec.entry_path.file_name().and_then(|s| s.to_str()), entry.file_name().and_then(|s| s.to_str()));
    }

    #[test]
    fn malformed_extension_sources_fail_gracefully(body in weird_string()) {
        // Wrap random body into a module shape so we exercise parser + hostcall bridge,
        // including malformed unicode/control content.
        let source = format!(
            "export default function activate(pi) {{ const marker = \"{body}\"; if (pi && marker.length > 1) {{ return marker; }}",
        );

        let result = load_extension_source(&source, PiJsRuntimeLimits::default());
        if let Err(err) = result {
            prop_assert!(!err.trim().is_empty());
        }
    }

    #[test]
    fn memory_limit_relaxation_is_monotonic(
        base_limit_kb in 256usize..1536,
        rounds in 128usize..768,
        width in 64usize..384,
    ) {
        let tight_limit = base_limit_kb * 1024;
        let relaxed_limit = tight_limit * 8;

        let tight = eval_with_memory_limit(tight_limit, rounds, width);
        let relaxed = eval_with_memory_limit(relaxed_limit, rounds, width);

        // If tight succeeds, relaxed must not fail on the same program.
        prop_assert!(!(tight.is_ok() && relaxed.is_err()));
    }

    #[test]
    fn extension_event_serialization_handles_edge_payloads(
        version in weird_string(),
        session_id in weird_string(),
        tool_name in weird_string(),
        tool_call_id in weird_string(),
        payload in json_value(),
    ) {
        let startup = ExtensionEvent::Startup {
            version,
            session_file: Some(session_id.clone()),
        };
        let agent_start = ExtensionEvent::AgentStart {
            session_id: session_id.clone(),
        };
        let tool_call = ExtensionEvent::ToolCall {
            tool_name,
            tool_call_id,
            input: payload,
        };
        let switch = ExtensionEvent::SessionBeforeSwitch {
            current_session: None,
            target_session: session_id,
        };

        for event in [startup, agent_start, tool_call, switch] {
            let encoded = serde_json::to_value(&event).expect("serialize extension event");
            let tag = encoded.get("type").and_then(Value::as_str).unwrap_or_default();
            prop_assert_eq!(tag, event.event_name());
        }
    }

    #[test]
    fn event_response_deserialization_is_total(value in json_value()) {
        let _ = serde_json::from_value::<InputEventResult>(value.clone());
        let _ = serde_json::from_value::<ToolResultEventResult>(value);
    }
}

// ── Dispatch-through-runtime safety tests ────────────────────────────────

/// Verify that `PiJsRuntime` enforces interrupt budget on infinite-loop scripts.
///
/// An extension executing `while (true) {}` must be terminated by the interrupt
/// handler, not allowed to run indefinitely.
#[test]
fn interrupt_budget_terminates_infinite_loop() {
    futures::executor::block_on(async {
        let config = PiJsRuntimeConfig {
            limits: PiJsRuntimeLimits {
                // Very small budget: forces termination quickly.
                interrupt_budget: Some(100),
                ..Default::default()
            },
            ..Default::default()
        };

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // This script would run forever without interrupt budget enforcement.
        let result = runtime.eval("while (true) {}").await;
        assert!(
            result.is_err(),
            "infinite loop should be terminated by interrupt budget, got: {result:?}"
        );
    });
}

/// Verify that hostcall requests dispatched through the JS runtime and completed
/// with various outcome types do not cause panics or undefined behavior.
#[test]
fn hostcall_complete_with_all_outcome_variants_is_safe() {
    use pi::scheduler::HostcallOutcome;

    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        // Enqueue multiple hostcalls from JS, each exercising a different kind.
        runtime
            .eval(
                r#"
                globalThis.results = {};
                pi.tool("read", { path: "/nonexistent" })
                    .then(r => { globalThis.results.tool = r; })
                    .catch(e => { globalThis.results.tool_err = e.message; });
                pi.exec("echo", { args: ["hello"] })
                    .then(r => { globalThis.results.exec = r; })
                    .catch(e => { globalThis.results.exec_err = e.message; });
                pi.log({ level: "info", message: "test" })
                    .then(r => { globalThis.results.log = r; })
                    .catch(e => { globalThis.results.log_err = e.message; });
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        assert!(
            requests.len() >= 2,
            "expected at least 2 hostcall requests, got {}",
            requests.len()
        );

        // Complete each with different outcome variants.
        let outcomes = [
            HostcallOutcome::Success(json!({"content": "test output"})),
            HostcallOutcome::Error {
                code: "denied".to_string(),
                message: "permission denied".to_string(),
            },
            HostcallOutcome::Success(Value::Null),
        ];

        for (req, outcome) in requests.into_iter().zip(outcomes.into_iter()) {
            runtime.complete_hostcall(&req.call_id, outcome);
        }

        // Tick the runtime to process completions — should not panic.
        let tick_result = runtime.tick().await;
        assert!(
            tick_result.is_ok(),
            "tick after completions should not fail"
        );
    });
}

/// Verify that completing a hostcall with an unknown `call_id` does not panic.
#[test]
fn hostcall_complete_unknown_call_id_is_safe() {
    use pi::scheduler::HostcallOutcome;

    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        // Complete a hostcall that was never enqueued — should be silently ignored.
        runtime.complete_hostcall(
            "nonexistent-call-id",
            HostcallOutcome::Error {
                code: "internal".to_string(),
                message: "stale completion".to_string(),
            },
        );

        // Tick should succeed without panics.
        let tick_result = runtime.tick().await;
        assert!(
            tick_result.is_ok(),
            "tick after unknown completion should not fail"
        );
    });
}
