//! Interactive first-time auth/credential setup wizard for the CLI surface.
//!
//! Contains the provider menu, credential collection, and OAuth flows that
//! run when Pi starts without valid credentials. Lives in the surface layer
//! because it is purely interactive I/O — no inference or session state.

use std::io::{self, Write};
use std::path::Path;

use anyhow::Result;
use pi::app::StartupError;
use pi::auth::{AuthCredential, AuthStorage};
use pi::cli;
use pi::config::Config;
use pi::provider_metadata;
use pi::tui::PiConsole;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupCredentialKind {
    ApiKey,
    OAuthPkce,
    OAuthDeviceFlow,
}

#[derive(Clone, Copy)]
pub struct ProviderChoice {
    pub provider: &'static str,
    pub label: &'static str,
    pub kind: SetupCredentialKind,
    pub env: &'static str,
}

pub const PROVIDER_CHOICES: &[ProviderChoice] = &[
    ProviderChoice {
        provider: "openai-codex",
        label: "OpenAI Codex (ChatGPT)",
        kind: SetupCredentialKind::OAuthPkce,
        env: "",
    },
    ProviderChoice {
        provider: "openai",
        label: "OpenAI",
        kind: SetupCredentialKind::ApiKey,
        env: "OPENAI_API_KEY",
    },
    ProviderChoice {
        provider: "anthropic",
        label: "Anthropic (Claude Code)",
        kind: SetupCredentialKind::OAuthPkce,
        env: "",
    },
    ProviderChoice {
        provider: "anthropic",
        label: "Anthropic (Claude API key)",
        kind: SetupCredentialKind::ApiKey,
        env: "ANTHROPIC_API_KEY",
    },
    ProviderChoice {
        provider: "kimi-for-coding",
        label: "Kimi for Coding",
        kind: SetupCredentialKind::OAuthDeviceFlow,
        env: "KIMI_API_KEY",
    },
    ProviderChoice {
        provider: "google-gemini-cli",
        label: "Google Cloud Code Assist",
        kind: SetupCredentialKind::OAuthPkce,
        env: "",
    },
    ProviderChoice {
        provider: "google",
        label: "Google Gemini",
        kind: SetupCredentialKind::ApiKey,
        env: "GOOGLE_API_KEY",
    },
    ProviderChoice {
        provider: "google-antigravity",
        label: "Google Antigravity",
        kind: SetupCredentialKind::OAuthPkce,
        env: "",
    },
    ProviderChoice {
        provider: "azure-openai",
        label: "Azure OpenAI",
        kind: SetupCredentialKind::ApiKey,
        env: "AZURE_OPENAI_API_KEY",
    },
    ProviderChoice {
        provider: "openrouter",
        label: "OpenRouter",
        kind: SetupCredentialKind::ApiKey,
        env: "OPENROUTER_API_KEY",
    },
];

pub fn provider_choice_default_for_provider(provider: &str) -> Option<ProviderChoice> {
    let canonical = provider_metadata::canonical_provider_id(provider).unwrap_or(provider);
    PROVIDER_CHOICES
        .iter()
        .copied()
        .find(|choice| choice.provider.eq_ignore_ascii_case(canonical))
}

pub fn provider_choice_from_token(token: &str) -> Option<ProviderChoice> {
    let raw = token.trim();
    let normalized = raw.to_ascii_lowercase();
    let (first, rest) = normalized
        .split_once(char::is_whitespace)
        .map_or((normalized.as_str(), ""), |(a, b)| (a, b.trim()));
    let wants_oauth = rest.contains("oauth");
    let wants_key = rest.contains("key") || rest.contains("api");
    let wants_explicit_method = wants_oauth || wants_key;
    let method_matches = |choice: &ProviderChoice| {
        (wants_oauth
            && matches!(
                choice.kind,
                SetupCredentialKind::OAuthPkce | SetupCredentialKind::OAuthDeviceFlow
            ))
            || (wants_key && choice.kind == SetupCredentialKind::ApiKey)
    };

    // Try numbered choice first (1-N).
    if let Ok(num) = first.parse::<usize>() {
        if num >= 1 && num <= PROVIDER_CHOICES.len() {
            return Some(PROVIDER_CHOICES[num - 1]);
        }
        return None;
    }

    // Try exact match against listed labels.
    for choice in PROVIDER_CHOICES {
        if normalized == choice.label.to_ascii_lowercase()
            || ((first == choice.provider || first == choice.provider.to_ascii_lowercase())
                && (!wants_explicit_method || method_matches(choice)))
        {
            return Some(*choice);
        }
    }

    // Common nicknames.
    match first {
        "codex" | "chatgpt" | "gpt" => return provider_choice_default_for_provider("openai-codex"),
        "claude" => return provider_choice_default_for_provider("anthropic"),
        "gemini" => return provider_choice_default_for_provider("google"),
        "kimi" => return provider_choice_default_for_provider("kimi-for-coding"),
        _ => {}
    }

    // Fall back to provider_metadata registry for any canonical ID or alias.
    let meta = provider_metadata::provider_metadata(first)?;
    let canonical = meta.canonical_id;

    // If we have an explicit method preference, honor it when multiple choices exist.
    if wants_explicit_method {
        if let Some(found) = PROVIDER_CHOICES.iter().copied().find(|choice| {
            choice.provider.eq_ignore_ascii_case(canonical) && method_matches(choice)
        }) {
            return Some(found);
        }
    }

    // Prefer known built-in flows for providers we support.
    if let Some(found) = provider_choice_default_for_provider(canonical) {
        return Some(found);
    }

    // Otherwise, fall back to API-key style with whatever env var hint we have.
    Some(ProviderChoice {
        provider: canonical,
        label: canonical,
        kind: SetupCredentialKind::ApiKey,
        env: meta.auth_env_keys.first().copied().unwrap_or(""),
    })
}

pub fn prompt_line(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    let bytes = io::stdin().read_line(&mut input)?;
    if bytes == 0 {
        return Ok(None);
    }
    Ok(Some(input.trim().to_string()))
}

#[allow(clippy::too_many_lines)]
pub async fn run_first_time_setup(
    startup_error: &StartupError,
    auth: &mut AuthStorage,
    cli: &mut cli::Cli,
    models_path: &Path,
) -> Result<bool> {
    let console = PiConsole::new();

    console.render_rule(Some("Welcome to Pi"));
    match startup_error {
        StartupError::NoModelsAvailable { .. } => {
            console.print_markup("[bold]No authenticated models are available yet.[/]\n");
        }
        StartupError::MissingApiKey { provider } => {
            console.print_markup(&format!(
                "[bold]Missing credentials for provider:[/] {provider}\n"
            ));
        }
    }
    console.print_markup("Let's authenticate.\n\n");

    let provider_hint = match startup_error {
        StartupError::MissingApiKey { provider } => provider_choice_from_token(provider),
        StartupError::NoModelsAvailable { .. } => {
            provider_choice_default_for_provider("openai-codex")
        }
    }
    .or_else(|| Some(PROVIDER_CHOICES[0]));

    console.print_markup("[bold]Choose a provider:[/]\n");
    for (idx, provider) in PROVIDER_CHOICES.iter().enumerate() {
        let is_default = provider_hint
            .is_some_and(|hint| hint.provider == provider.provider && hint.kind == provider.kind);
        let default_marker = if is_default { " [dim](default)[/]" } else { "" };
        let method = match provider.kind {
            SetupCredentialKind::ApiKey => "API key",
            SetupCredentialKind::OAuthPkce => "OAuth",
            SetupCredentialKind::OAuthDeviceFlow => "OAuth (device flow)",
        };
        let hint = if provider.env.trim().is_empty() {
            method.to_string()
        } else {
            format!("{method}  {}", provider.env)
        };
        console.print_markup(&format!(
            "  [cyan]{})[/] {}  [dim]{}[/]{}\n",
            idx + 1,
            provider.label,
            hint,
            default_marker
        ));
    }
    let num_choices = PROVIDER_CHOICES.len();
    console.print_markup(&format!(
        "  [cyan]{})[/] Custom provider via models.json\n",
        num_choices + 1
    ));
    console.print_markup(&format!("  [cyan]{})[/] Exit setup\n\n", num_choices + 2));
    console
        .print_markup("[dim]Or type any provider name (e.g., deepseek, cerebras, ollama).[/]\n\n");

    let custom_num = (num_choices + 1).to_string();
    let exit_num = (num_choices + 2).to_string();
    let provider = loop {
        let prompt = provider_hint.map_or_else(
            || format!("Select 1-{} or provider name: ", num_choices + 2),
            |default_provider| {
                format!(
                    "Select 1-{} or name (Enter for {}): ",
                    num_choices + 2,
                    default_provider.label
                )
            },
        );
        let Some(input) = prompt_line(&prompt)? else {
            console.render_warning("Setup cancelled (no input).");
            return Ok(false);
        };
        let normalized = input.trim().to_lowercase();
        if normalized.is_empty() {
            if let Some(default_provider) = provider_hint {
                break default_provider;
            }
            continue;
        }
        if normalized == custom_num || normalized == "custom" || normalized == "models" {
            console.render_info(&format!(
                "Create models.json at {} and restart Pi.",
                models_path.display()
            ));
            return Ok(false);
        }
        if normalized == exit_num
            || normalized == "q"
            || normalized == "quit"
            || normalized == "exit"
        {
            console.render_warning("Setup cancelled.");
            return Ok(false);
        }
        if let Some(provider) = provider_choice_from_token(&normalized) {
            break provider;
        }
        console.render_warning("Unrecognized choice. Please try again.");
    };

    let credential = match provider.kind {
        SetupCredentialKind::ApiKey => {
            console.print_markup("Paste your API key (input will be visible):\n");
            let Some(raw_key) = prompt_line("API key: ")? else {
                console.render_warning("Setup cancelled (no input).");
                return Ok(false);
            };
            let key = raw_key.trim();
            if key.is_empty() {
                console.render_warning("No API key entered. Setup cancelled.");
                return Ok(false);
            }

            AuthCredential::ApiKey {
                key: key.to_string(),
            }
        }
        SetupCredentialKind::OAuthPkce => {
            let start = match provider.provider {
                "openai-codex" => pi::auth::start_openai_codex_oauth()?,
                "anthropic" => pi::auth::start_anthropic_oauth()?,
                "google-gemini-cli" => pi::auth::start_google_gemini_cli_oauth()?,
                "google-antigravity" => pi::auth::start_google_antigravity_oauth()?,
                _ => {
                    console.render_warning(&format!(
                        "OAuth login is not supported for {} in this setup flow. Start Pi and run /login {} instead.",
                        provider.provider, provider.provider
                    ));
                    return Ok(false);
                }
            };

            if start.provider == "anthropic" {
                console.render_warning(
                    "Anthropic OAuth (Claude Code consumer account) is no longer recommended.\n\
Using consumer OAuth tokens outside the official client may violate Anthropic's consumer Terms of Service and can\n\
result in account suspension/ban. Prefer using an Anthropic API key (ANTHROPIC_API_KEY) instead.",
                );
            }

            console.print_markup(&format!(
                "[bold]OAuth login:[/] {}\n\nOpen this URL:\n{}\n\n{}\n",
                start.provider,
                start.url,
                start.instructions.as_deref().unwrap_or_default()
            ));
            let Some(code_input) = prompt_line("Paste callback URL or code: ")? else {
                console.render_warning("Setup cancelled (no input).");
                return Ok(false);
            };
            let code_input = code_input.trim();
            if code_input.is_empty() {
                console.render_warning("No authorization code provided. Setup cancelled.");
                return Ok(false);
            }

            match start.provider.as_str() {
                "openai-codex" => {
                    pi::auth::complete_openai_codex_oauth(code_input, &start.verifier).await?
                }
                "anthropic" => {
                    pi::auth::complete_anthropic_oauth(code_input, &start.verifier).await?
                }
                "google-gemini-cli" => {
                    pi::auth::complete_google_gemini_cli_oauth(code_input, &start.verifier).await?
                }
                "google-antigravity" => {
                    pi::auth::complete_google_antigravity_oauth(code_input, &start.verifier).await?
                }
                other => {
                    console.render_warning(&format!(
                        "OAuth completion not supported for {other}. Setup cancelled."
                    ));
                    return Ok(false);
                }
            }
        }
        SetupCredentialKind::OAuthDeviceFlow => {
            if provider.provider != "kimi-for-coding" {
                console.render_warning(&format!(
                    "Device-flow login not supported for {} in this setup flow. Start Pi and run /login {} instead.",
                    provider.provider, provider.provider
                ));
                return Ok(false);
            }

            let device = pi::auth::start_kimi_code_device_flow().await?;
            let verification_url = device
                .verification_uri_complete
                .clone()
                .unwrap_or_else(|| device.verification_uri.clone());
            console.print_markup(&format!(
                "[bold]OAuth login:[/] kimi-for-coding\n\n\
Open this URL:\n{verification_url}\n\n\
If prompted, enter this code: {}\n\
Code expires in {} seconds.\n",
                device.user_code, device.expires_in
            ));

            let start = std::time::Instant::now();
            loop {
                let elapsed = start.elapsed().as_secs();
                if elapsed >= device.expires_in {
                    console.render_warning("Device code expired. Run setup again.");
                    return Ok(false);
                }

                let Some(input) = prompt_line("Press Enter to poll (or type q to cancel): ")?
                else {
                    console.render_warning("Setup cancelled (no input).");
                    return Ok(false);
                };
                if input.trim().eq_ignore_ascii_case("q") {
                    console.render_warning("Setup cancelled.");
                    return Ok(false);
                }

                match pi::auth::poll_kimi_code_device_flow(&device.device_code).await {
                    pi::auth::DeviceFlowPollResult::Success(cred) => break cred,
                    pi::auth::DeviceFlowPollResult::Pending => {
                        console.render_info("Authorization still pending. Complete the browser step and poll again.");
                    }
                    pi::auth::DeviceFlowPollResult::SlowDown => {
                        console.render_info("Authorization server asked to slow down. Wait a few seconds and poll again.");
                    }
                    pi::auth::DeviceFlowPollResult::Expired => {
                        console.render_warning("Device code expired. Run setup again.");
                        return Ok(false);
                    }
                    pi::auth::DeviceFlowPollResult::AccessDenied => {
                        console.render_warning("Access denied. Run setup again.");
                        return Ok(false);
                    }
                    pi::auth::DeviceFlowPollResult::Error(err) => {
                        console.render_warning(&format!("OAuth polling failed: {err}"));
                        return Ok(false);
                    }
                }
            }
        }
    };

    auth.set_login_credential(provider.provider, credential);
    auth.save_async().await?;

    // Make the next startup attempt use the credential we just created.
    if cli.provider.as_deref() != Some(provider.provider) {
        cli.provider = Some(provider.provider.to_string());
        cli.model = None;
    }
    if provider.provider == "openai-codex" {
        cli.model = Some("gpt-5.3-codex".to_string());
    }

    let saved_label = match provider.kind {
        SetupCredentialKind::ApiKey => "API key",
        SetupCredentialKind::OAuthPkce | SetupCredentialKind::OAuthDeviceFlow => {
            "OAuth credentials"
        }
    };
    console.render_success(&format!(
        "Saved {label} for {provider} to {path}",
        label = saved_label,
        provider = provider.provider,
        path = Config::auth_path().display()
    ));
    console.render_info("Continuing startup...");
    Ok(true)
}
