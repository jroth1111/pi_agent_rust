//! Pi - High-performance AI coding agent CLI
//!
//! Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance: Sub-100ms startup, smooth TUI at 60fps
//! - Reliability: No panics in normal operation
//! - Efficiency: Single binary, minimal dependencies

#![forbid(unsafe_code)]
// Allow dead code and unused async during scaffolding phase - remove once implementation is complete
#![allow(
    dead_code,
    clippy::unused_async,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::uninlined_format_args,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value,
    clippy::needless_pass_by_ref_mut,
    clippy::collapsible_match,
    clippy::default_trait_access,
    clippy::field_reassign_with_default,
    clippy::items_after_statements,
    clippy::manual_let_else,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::unused_self
)]

use std::io;
use std::path::PathBuf;

use anyhow::Result;
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
use clap::error::ErrorKind;
use pi::cli;
use pi::config::Config;
use pi::contracts::ApplicationKernel;
#[cfg(test)]
use pi::surface::auth_setup::{SetupCredentialKind, provider_choice_from_token};
use pi::surface::cli_commands::{
    handle_fast_path, handle_subcommand, list_models, validate_theme_path_spec,
};
use tracing_subscriber::EnvFilter;

const EXIT_CODE_FAILURE: i32 = 1;
const EXIT_CODE_USAGE: i32 = 2;
const USAGE_ERROR_PATTERNS: &[&str] = &[
    "@file arguments are not supported in rpc mode",
    "--api-key requires a model to be specified via --provider/--model or --models",
    "unknown --only categories",
    "--only must include at least one category",
    "theme file not found",
    "theme spec is empty",
];

fn main() {
    if let Err(err) = main_impl() {
        let exit_code = exit_code_for_error(&err);
        print_error_with_hints(&err);
        std::process::exit(exit_code);
    }
}

fn parse_cli_args(raw_args: Vec<String>) -> Result<Option<(cli::Cli, Vec<cli::ExtensionCliFlag>)>> {
    match cli::parse_with_extension_flags(raw_args) {
        Ok(parsed) => Ok(Some((parsed.cli, parsed.extension_flags))),
        Err(err) => {
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                err.print()?;
                return Ok(None);
            }
            Err(anyhow::Error::new(err))
        }
    }
}

fn parse_cli_from_env() -> Result<Option<(cli::Cli, Vec<cli::ExtensionCliFlag>)>> {
    parse_cli_args(std::env::args().collect())
}

#[allow(clippy::too_many_lines)]
fn main_impl() -> Result<()> {
    let Some((cli, extension_flags)) = parse_cli_from_env()? else {
        return Ok(());
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    validate_theme_path_spec(cli.theme.as_deref(), &cwd)?;

    if handle_fast_path(&cli, &cwd)? {
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .with_writer(io::stderr)
        .init();

    let reactor = create_reactor()?;
    let runtime = RuntimeBuilder::multi_thread()
        .blocking_threads(1, 8)
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let handle = runtime.handle();
    let runtime_handle = handle.clone();
    let join = handle.spawn(Box::pin(run(cli, extension_flags, runtime_handle)));
    runtime.block_on(join)
}

fn print_error_with_hints(err: &anyhow::Error) {
    for cause in err.chain() {
        if let Some(pi_error) = cause.downcast_ref::<pi::error::Error>() {
            eprint!("{}", pi::error_hints::format_error_with_hints(pi_error));
            return;
        }
    }

    eprintln!("{err}");
}

fn exit_code_for_error(err: &anyhow::Error) -> i32 {
    if is_usage_error(err) {
        EXIT_CODE_USAGE
    } else {
        EXIT_CODE_FAILURE
    }
}

fn is_usage_error(err: &anyhow::Error) -> bool {
    if err
        .chain()
        .any(|cause| cause.downcast_ref::<clap::Error>().is_some())
    {
        return true;
    }

    if err.chain().any(|cause| {
        cause
            .downcast_ref::<pi::error::Error>()
            .is_some_and(|pi_error| matches!(pi_error, pi::error::Error::Validation(_)))
    }) {
        return true;
    }

    let message = err.to_string().to_ascii_lowercase();
    USAGE_ERROR_PATTERNS
        .iter()
        .any(|pattern| message.contains(pattern))
}

fn apply_cli_retry_override(config: &mut Config, cli: &cli::Cli) {
    if !cli.retry {
        return;
    }

    let mut retry = config.retry.clone().unwrap_or_default();
    retry.enabled = Some(true);
    config.retry = Some(retry);
}

#[allow(clippy::too_many_lines)]
async fn run(
    mut cli: cli::Cli,
    extension_flags: Vec<cli::ExtensionCliFlag>,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if let Some(command) = cli.command.take() {
        handle_subcommand(command, &cwd).await?;
        return Ok(());
    }

    if !cli.no_migrations {
        let migration_report = pi::migrations::run_startup_migrations(&cwd);
        for message in migration_report.messages() {
            eprintln!("{message}");
        }
    }

    ApplicationKernel::new(runtime_handle, cwd, list_models)
        .run_cli_surface(cli, extension_flags)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use clap::Parser;
    use pi::agent::AgentEvent;
    use pi::model::StopReason;
    use serde_json::Value;

    /// Test helper: exponential backoff delay for print-mode retry.
    fn print_mode_retry_delay_ms(config: &Config, attempt: u32) -> u32 {
        let base = u64::from(config.retry_base_delay_ms());
        let max = u64::from(config.retry_max_delay_ms());
        let shift = attempt.saturating_sub(1);
        let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        let delay = base.saturating_mul(multiplier).min(max);
        u32::try_from(delay).unwrap_or(u32::MAX)
    }

    /// Test helper: checks whether an assistant message represents a retryable error.
    fn is_retryable_prompt_result(msg: &pi::model::AssistantMessage) -> bool {
        if !matches!(msg.stop_reason, StopReason::Error) {
            return false;
        }
        let err_msg = msg.error_message.as_deref().unwrap_or("Request error");
        pi::error::is_retryable_error(err_msg, Some(msg.usage.input), None)
    }

    #[test]
    fn exit_code_classifier_marks_usage_errors() {
        let usage_err = anyhow!("Unknown --only categories: nope");
        assert_eq!(exit_code_for_error(&usage_err), EXIT_CODE_USAGE);

        let validation_err = anyhow::Error::new(pi::error::Error::validation("bad input"));
        assert_eq!(exit_code_for_error(&validation_err), EXIT_CODE_USAGE);
    }

    #[test]
    fn exit_code_classifier_defaults_to_general_failure() {
        let runtime_err = anyhow::Error::new(pi::error::Error::auth("missing key"));
        assert_eq!(exit_code_for_error(&runtime_err), EXIT_CODE_FAILURE);
    }

    #[test]
    fn parse_cli_args_extracts_extension_flags() {
        let parsed = parse_cli_args(vec![
            "pi".to_string(),
            "--model".to_string(),
            "gpt-4o".to_string(),
            "--plan".to_string(),
            "ship-it".to_string(),
            "--dry-run".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse args")
        .expect("parsed cli payload");

        assert_eq!(parsed.0.model.as_deref(), Some("gpt-4o"));
        assert!(parsed.0.print);
        assert_eq!(parsed.1.len(), 2);
        assert_eq!(parsed.1[0].name, "plan");
        assert_eq!(parsed.1[0].value.as_deref(), Some("ship-it"));
        assert_eq!(parsed.1[1].name, "dry-run");
        assert!(parsed.1[1].value.is_none());
    }

    #[test]
    fn parse_cli_args_keeps_subcommand_validation() {
        let result = parse_cli_args(vec![
            "pi".to_string(),
            "install".to_string(),
            "--bogus".to_string(),
            "pkg".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn coerce_extension_flag_bool_defaults_to_true_without_value() {
        let flag = cli::ExtensionCliFlag {
            name: "dry-run".to_string(),
            value: None,
        };
        let value = pi::surface::extension_runtime::coerce_extension_flag_value(&flag, "bool")
            .expect("coerce bool");
        assert_eq!(value, Value::Bool(true));
    }

    #[test]
    fn coerce_extension_flag_rejects_invalid_bool_text() {
        let flag = cli::ExtensionCliFlag {
            name: "dry-run".to_string(),
            value: Some("maybe".to_string()),
        };
        let err = pi::surface::extension_runtime::coerce_extension_flag_value(&flag, "bool")
            .expect_err("invalid bool should fail");
        assert!(err.to_string().contains("Invalid boolean value"));
    }

    #[test]
    fn provider_choice_from_token_numbered_choices() {
        let choice = provider_choice_from_token("1").expect("provider 1");
        assert_eq!(choice.provider, "openai-codex");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("2").expect("provider 2");
        assert_eq!(choice.provider, "openai");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("3").expect("provider 3");
        assert_eq!(choice.provider, "anthropic");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("4").expect("provider 4");
        assert_eq!(choice.provider, "anthropic");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("5").expect("provider 5");
        assert_eq!(choice.provider, "kimi-for-coding");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthDeviceFlow);

        let choice = provider_choice_from_token("6").expect("provider 6");
        assert_eq!(choice.provider, "google-gemini-cli");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("7").expect("provider 7");
        assert_eq!(choice.provider, "google");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("8").expect("provider 8");
        assert_eq!(choice.provider, "google-antigravity");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("9").expect("provider 9");
        assert_eq!(choice.provider, "azure-openai");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("10").expect("provider 10");
        assert_eq!(choice.provider, "openrouter");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);
        // Out of range
        assert!(provider_choice_from_token("0").is_none());
        assert!(provider_choice_from_token("11").is_none());
    }

    #[test]
    fn provider_choice_from_token_common_nicknames() {
        assert_eq!(
            provider_choice_from_token("claude").unwrap().provider,
            "anthropic"
        );
        assert_eq!(
            provider_choice_from_token("gpt").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(
            provider_choice_from_token("chatgpt").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(
            provider_choice_from_token("gemini").unwrap().provider,
            "google"
        );
        assert_eq!(
            provider_choice_from_token("kimi").unwrap().provider,
            "kimi-for-coding"
        );
    }

    #[test]
    fn provider_choice_from_token_canonical_ids() {
        assert_eq!(
            provider_choice_from_token("anthropic").unwrap().provider,
            "anthropic"
        );
        assert_eq!(
            provider_choice_from_token("openai").unwrap().provider,
            "openai"
        );
        assert_eq!(
            provider_choice_from_token("openai-codex").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(provider_choice_from_token("groq").unwrap().provider, "groq");
        assert_eq!(
            provider_choice_from_token("openrouter").unwrap().provider,
            "openrouter"
        );
        assert_eq!(
            provider_choice_from_token("mistral").unwrap().provider,
            "mistral"
        );
    }

    #[test]
    fn provider_choice_from_token_case_insensitive() {
        assert_eq!(
            provider_choice_from_token("ANTHROPIC").unwrap().provider,
            "anthropic"
        );
        assert_eq!(provider_choice_from_token("Groq").unwrap().provider, "groq");
        assert_eq!(
            provider_choice_from_token("OpenRouter").unwrap().provider,
            "openrouter"
        );
    }

    #[test]
    fn provider_choice_from_token_metadata_fallback() {
        // Providers not in the top-10 list but in provider_metadata registry
        assert_eq!(
            provider_choice_from_token("deepseek").unwrap().provider,
            "deepseek"
        );
        assert_eq!(
            provider_choice_from_token("cerebras").unwrap().provider,
            "cerebras"
        );
        assert_eq!(
            provider_choice_from_token("cohere").unwrap().provider,
            "cohere"
        );
        assert_eq!(
            provider_choice_from_token("perplexity").unwrap().provider,
            "perplexity"
        );
        // Aliases resolve through metadata
        assert_eq!(
            provider_choice_from_token("open-router").unwrap().provider,
            "openrouter"
        );
        assert_eq!(
            provider_choice_from_token("dashscope").unwrap().provider,
            "alibaba"
        );
    }

    #[test]
    fn provider_choice_from_token_honors_method_preference() {
        let provider = provider_choice_from_token("anthropic oauth").expect("anthropic oauth");
        assert_eq!(provider.provider, "anthropic");
        assert_eq!(provider.kind, SetupCredentialKind::OAuthPkce);

        let provider = provider_choice_from_token("anthropic key").expect("anthropic key");
        assert_eq!(provider.provider, "anthropic");
        assert_eq!(provider.kind, SetupCredentialKind::ApiKey);
    }

    #[test]
    fn provider_choice_from_token_whitespace_handling() {
        assert_eq!(
            provider_choice_from_token("  groq  ").unwrap().provider,
            "groq"
        );
        assert_eq!(
            provider_choice_from_token(" 1 ").unwrap().provider,
            "openai-codex"
        );
    }

    #[test]
    fn provider_choice_from_token_unknown_returns_none() {
        assert!(provider_choice_from_token("nonexistent-provider-xyz").is_none());
        assert!(provider_choice_from_token("").is_none());
    }

    // ================================================================
    // Retry helper tests
    // ================================================================

    #[test]
    fn print_mode_retry_delay_first_attempt_is_base() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(3),
                reviewer_model: None,
                token_budget: None,
                base_delay_ms: Some(2000),
                max_delay_ms: Some(60_000),
            }),
            ..Config::default()
        };
        assert_eq!(print_mode_retry_delay_ms(&config, 1), 2000);
    }

    #[test]
    fn cli_retry_override_enables_retry_when_missing() {
        let mut config = Config::default();
        let mut cli = cli::Cli::parse_from(["pi"]);
        cli.retry = true;

        apply_cli_retry_override(&mut config, &cli);
        assert!(config.retry_enabled());
        assert_eq!(config.retry_max_retries(), 3);
    }

    #[test]
    fn cli_retry_override_preserves_existing_retry_settings() {
        let mut config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(false),
                max_retries: Some(5),
                reviewer_model: Some("reviewer-x".to_string()),
                token_budget: Some(42_000),
                base_delay_ms: Some(250),
                max_delay_ms: Some(4_000),
            }),
            ..Config::default()
        };
        let mut cli = cli::Cli::parse_from(["pi"]);
        cli.retry = true;

        apply_cli_retry_override(&mut config, &cli);

        assert!(config.retry_enabled());
        assert_eq!(config.retry_max_retries(), 5);
        assert_eq!(config.retry_base_delay_ms(), 250);
        assert_eq!(config.retry_max_delay_ms(), 4_000);
        assert_eq!(config.retry_reviewer_model().as_deref(), Some("reviewer-x"));
        assert_eq!(config.retry_token_budget(), Some(42_000));
    }

    #[test]
    fn print_mode_retry_delay_doubles_each_attempt() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(5),
                reviewer_model: None,
                token_budget: None,
                base_delay_ms: Some(1000),
                max_delay_ms: Some(60_000),
            }),
            ..Config::default()
        };
        assert_eq!(print_mode_retry_delay_ms(&config, 2), 2000);
        assert_eq!(print_mode_retry_delay_ms(&config, 3), 4000);
    }

    #[test]
    fn print_mode_retry_delay_capped_at_max() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(10),
                reviewer_model: None,
                token_budget: None,
                base_delay_ms: Some(2000),
                max_delay_ms: Some(10_000),
            }),
            ..Config::default()
        };
        let delay = print_mode_retry_delay_ms(&config, 5);
        assert!(delay <= 10_000, "delay {delay} should be capped at 10000");
    }

    #[test]
    fn is_retryable_prompt_result_identifies_retryable_errors() {
        use pi::model::{AssistantMessage, Usage};

        let retryable = AssistantMessage {
            content: vec![],
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("429 rate limit exceeded".to_string()),
            timestamp: 0,
        };
        assert!(is_retryable_prompt_result(&retryable));

        let not_retryable = AssistantMessage {
            error_message: Some("invalid api key".to_string()),
            ..retryable.clone()
        };
        assert!(!is_retryable_prompt_result(&not_retryable));

        let success = AssistantMessage {
            stop_reason: StopReason::Stop,
            error_message: None,
            ..retryable
        };
        assert!(!is_retryable_prompt_result(&success));
    }

    #[test]
    fn emit_json_event_serializes_retry_events() {
        let start = AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "rate limited".to_string(),
        };
        let json = serde_json::to_value(&start).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 1);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 2000);

        let end = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        };
        let json = serde_json::to_value(&end).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert!(json["success"].as_bool().unwrap());
    }
}
