//! Pi - High-performance AI coding agent CLI
//!
//! This library provides the core functionality for the Pi CLI tool,
//! a Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance: Sub-100ms startup, smooth TUI at 60fps
//! - Reliability: No panics in normal operation
//! - Efficiency: Single binary, minimal dependencies
//!
//! ## Public API policy
//!
//! The `pi` crate is primarily the implementation crate for the `pi` CLI binary.
//! External consumers should treat non-`sdk` modules/types as **unstable**
//! and subject to change. Use [`sdk`] as the stable library-facing surface.
//!
//! Currently intended stable exports:
//! - [`Error`]
//! - [`PiResult`]
//! - [`sdk`] module

#![forbid(unsafe_code)]
#![allow(dead_code, clippy::unused_async, unused_attributes)]
// Allow pedantic lints during early development - can tighten later
#![allow(
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::large_stack_arrays,
    clippy::large_stack_frames,
    clippy::module_name_repetitions,
    clippy::similar_names,
    clippy::wildcard_imports,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cloned_ref_to_slice_refs,
    clippy::collapsible_match,
    clippy::default_constructed_unit_structs,
    clippy::default_trait_access,
    clippy::field_reassign_with_default,
    clippy::float_cmp,
    clippy::format_push_string,
    clippy::ignored_unit_patterns,
    clippy::implicit_clone,
    clippy::implicit_hasher,
    clippy::items_after_statements,
    clippy::len_zero,
    clippy::manual_clamp,
    clippy::manual_let_else,
    clippy::manual_string_new,
    clippy::manual_strip,
    clippy::map_unwrap_or,
    clippy::match_same_arms,
    clippy::missing_fields_in_debug,
    clippy::needless_continue,
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::needless_range_loop,
    clippy::non_std_lazy_statics,
    clippy::only_used_in_recursion,
    clippy::option_if_let_else,
    clippy::or_fun_call,
    clippy::redundant_clone,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::ref_option,
    clippy::return_self_not_must_use,
    clippy::self_only_used_in_recursion,
    clippy::should_implement_trait,
    clippy::significant_drop_tightening,
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unnecessary_get_then_check,
    clippy::unnecessary_wraps,
    clippy::unused_self,
    clippy::use_self
)]

// Allow in-crate tests that include integration test helpers to resolve `pi::...`
// paths the same way integration tests do.
extern crate self as pi;

// Gap H: jemalloc allocator for allocation-heavy paths.
// Declared in the library so all project binaries/tests share allocator behavior.
#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub mod agent;
pub mod agent_cx;
pub mod app;
pub mod auth;
pub mod autocomplete;
pub mod buffer_shim;
pub mod cli;
pub mod compaction;
pub mod compaction_worker;
pub mod config;
pub mod conformance;
pub mod conformance_shapes;
pub mod connectors;
pub mod context;
pub mod crypto_shim;
pub mod doctor;
pub mod error;
pub mod error_hints;
pub mod events;
pub mod extension_conformance_matrix;
pub mod extension_dispatcher;
pub mod extension_events;
pub mod extension_inclusion;
pub mod extension_index;
pub mod extension_license;
pub mod extension_popularity;
pub mod extension_preflight;
pub mod extension_replay;
pub mod extension_scoring;
pub mod extension_tools;
pub mod extension_validation;
pub mod extensions;
pub mod extensions_js;
pub mod flake_classifier;
pub mod hostcall_amac;
pub mod hostcall_io_uring_lane;
pub mod hostcall_queue;
pub mod hostcall_rewrite;
pub mod hostcall_s3_fifo;
pub mod hostcall_superinstructions;
pub mod hostcall_trace_jit;
pub mod http;
pub mod http_shim;
pub mod interactive;
pub mod keybindings;
pub mod lsp;
pub mod migrations;
pub mod model;
pub mod model_selector;
pub mod models;
pub mod orchestration;
pub mod package_manager;
pub mod perf_build;
pub mod permissions;
#[cfg(feature = "wasm-host")]
pub mod pi_wasm;
pub mod policy;
pub mod provider;
pub mod provider_metadata;
pub mod providers;
pub mod reliability;
pub mod resources;
pub mod rpc;
pub mod runtime;
pub mod sandbox;
pub mod scheduler;
pub mod sdk;
pub mod session;
pub mod session_index;
pub mod session_metrics;
pub mod session_picker;
#[cfg(feature = "sqlite-sessions")]
pub mod session_sqlite;
pub mod session_store_v2;
pub mod skills;
pub mod sse;
pub mod state;
pub mod task_graph;
pub mod terminal_images;
pub mod theme;
pub mod tools;
pub mod tui;
pub mod vcr;
pub mod verification;
pub mod version_check;

pub use error::{Error, Result as PiResult};
pub use extension_dispatcher::ExtensionDispatcher;

// Conditional re-exports for fuzz harnesses.
// These expose internal parsing functions that are normally private,
// gated behind the `fuzzing` feature so they do not appear in the
// public API during normal builds.
#[cfg(feature = "fuzzing")]
pub mod fuzz_exports {
    //! Re-exports of internal parsing/deserialization functions for
    //! `cargo-fuzz` / `libFuzzer` harnesses.
    //!
    //! Enabled only when the `fuzzing` Cargo feature is active.
    //! The `fuzz/Cargo.toml` depends on this crate with
    //! `features = ["fuzzing"]`.

    pub use crate::config::Config;
    pub use crate::model::{
        AssistantMessage, ContentBlock, Message, StreamEvent, TextContent, ThinkingContent,
        ToolCall, ToolResultMessage, Usage, UserContent, UserMessage,
    };
    pub use crate::session::{Session, SessionEntry, SessionHeader, SessionMessage};
    pub use crate::sse::{SseEvent, SseParser};
    pub use crate::tools::{fuzz_normalize_dot_segments, fuzz_resolve_path};

    // Provider stream processor wrappers for coverage-guided fuzzing.
    pub use crate::providers::anthropic::fuzz::Processor as AnthropicProcessor;
    pub use crate::providers::azure::fuzz::Processor as AzureProcessor;
    pub use crate::providers::cohere::fuzz::Processor as CohereProcessor;
    pub use crate::providers::gemini::fuzz::Processor as GeminiProcessor;
    pub use crate::providers::openai::fuzz::Processor as OpenAIProcessor;
    pub use crate::providers::openai_responses::fuzz::Processor as OpenAIResponsesProcessor;
    pub use crate::providers::vertex::fuzz::Processor as VertexProcessor;
}

#[cfg(test)]
mod main {
    mod tests {
        use crate::config::{Config, ReliabilityConfig, ReliabilityEnforcementMode};
        use crate::tools::ToolRegistry;
        use futures::executor::block_on;
        use serde_json::{Value, json};
        use tempfile::TempDir;

        fn config_with_reliability(enabled: bool, mode: ReliabilityEnforcementMode) -> Config {
            Config {
                reliability: Some(ReliabilityConfig {
                    enabled: Some(enabled),
                    enforcement_mode: Some(mode),
                    ..Default::default()
                }),
                ..Config::default()
            }
        }

        fn extract_exec_decision(details: Option<&Value>) -> Option<String> {
            details
                .and_then(|v| v.get("execMediation"))
                .and_then(|v| v.get("decision"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        }

        #[test]
        fn reliability_mode_boot_smoke() {
            let tmp = TempDir::new().expect("tempdir");
            let tools = ["bash"];

            let disabled = config_with_reliability(false, ReliabilityEnforcementMode::Observe);
            let observe = config_with_reliability(true, ReliabilityEnforcementMode::Observe);

            assert!(!disabled.reliability_enabled());
            assert!(observe.reliability_enabled());
            assert_eq!(
                observe.reliability_enforcement_mode(),
                ReliabilityEnforcementMode::Observe
            );

            let disabled_registry = ToolRegistry::new(&tools, tmp.path(), Some(&disabled));
            let observe_registry = ToolRegistry::new(&tools, tmp.path(), Some(&observe));

            let disabled_bash = disabled_registry.get("bash").expect("bash tool");
            let observe_bash = observe_registry.get("bash").expect("bash tool");

            // Dangerous command should be mediated identically in disabled and observe modes.
            let request = json!({ "command": "rm -rf /", "timeout": 1 });
            let disabled_out =
                block_on(disabled_bash.execute("call-disabled", request.clone(), None))
                    .expect("disabled run");
            let observe_out =
                block_on(observe_bash.execute("call-observe", request, None)).expect("observe run");

            assert_eq!(disabled_out.is_error, observe_out.is_error);
            assert_eq!(
                extract_exec_decision(disabled_out.details.as_ref()),
                extract_exec_decision(observe_out.details.as_ref())
            );
        }
    }
}
