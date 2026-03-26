//! Concrete application composition root for CLI surface startup.
//!
//! The previous builder required a full control-plane implementation that does
//! not exist in this repo, so it could never become the active assembly path.
//! This kernel is the real composition root for the live CLI startup flow:
//! `main.rs` hands the parsed CLI to this type, and the kernel drives surface
//! bootstrap, model selection, agent-session assembly, and final route dispatch.

use std::path::PathBuf;

use anyhow::Result;
use asupersync::runtime::RuntimeHandle;

use crate::cli;
use crate::config::Config;
use crate::error::Error;
use crate::models::ModelRegistry;
use crate::session::Session;
use crate::surface::extension_runtime::{
    resolve_surface_extension_runtime_flavor, spawn_surface_extension_prewarm,
};
use crate::surface::routing::run_selected_surface_route;
use crate::surface::startup::{
    PreparedSurfaceInputs, StartupResources, SurfaceSelectionOutcome, build_surface_agent_session,
    flush_session_autosave_on_shutdown, load_surface_startup_resources, prepare_surface_inputs,
    refresh_surface_startup_auth, resolve_surface_model_selection, spawn_session_index_maintenance,
};

type ListModelsFn = fn(&ModelRegistry, Option<&str>);

/// Active runtime composition root for CLI-driven surface execution.
#[derive(Clone)]
pub struct ApplicationKernel {
    runtime_handle: RuntimeHandle,
    cwd: PathBuf,
    list_models: ListModelsFn,
}

impl ApplicationKernel {
    #[must_use]
    pub fn new(runtime_handle: RuntimeHandle, cwd: PathBuf, list_models: ListModelsFn) -> Self {
        Self {
            runtime_handle,
            cwd,
            list_models,
        }
    }

    #[must_use]
    pub fn builder() -> ApplicationKernelBuilder {
        ApplicationKernelBuilder::default()
    }

    /// Run the authoritative CLI surface bootstrap and route dispatch flow.
    ///
    /// This is the live composition path for the CLI binary. It owns startup
    /// orchestration and delegates subordinate work to surface helpers.
    pub async fn run_cli_surface(
        &self,
        mut cli: cli::Cli,
        extension_flags: Vec<cli::ExtensionCliFlag>,
    ) -> Result<()> {
        spawn_session_index_maintenance();

        let StartupResources {
            config,
            resources,
            mut auth,
            resource_cli,
        } = load_surface_startup_resources(&cli, &extension_flags, &self.cwd).await?;

        let extension_runtime_flavor = resolve_surface_extension_runtime_flavor(&resources)?;
        let extension_prewarm_handle = spawn_surface_extension_prewarm(
            &self.runtime_handle,
            &cli,
            &config,
            &self.cwd,
            extension_runtime_flavor,
        );

        refresh_surface_startup_auth(&mut auth).await?;
        let package_dir = Config::package_dir();

        let Some(PreparedSurfaceInputs {
            global_dir,
            models_path,
            mut model_registry,
            initial,
            messages,
            surface_bootstrap,
            mode,
            non_interactive_guard,
            scoped_patterns,
        }) = prepare_surface_inputs(&mut cli, &config, &auth, &self.cwd, self.list_models).await?
        else {
            return Ok(());
        };

        let session = Box::pin(Session::new(&cli, &config)).await?;

        let (selection, resolved_key) = match resolve_surface_model_selection(
            &surface_bootstrap,
            non_interactive_guard.as_ref(),
            &mut auth,
            &mut cli,
            &models_path,
            &mut model_registry,
            &scoped_patterns,
            &global_dir,
            &config,
            &session,
        )
        .await?
        {
            SurfaceSelectionOutcome::Selected(resolution) => {
                (resolution.selection, resolution.resolved_key)
            }
            SurfaceSelectionOutcome::ExitQuietly => return Ok(()),
        };

        let enabled_tools = cli.enabled_tools();
        let skills_prompt = if enabled_tools.contains(&"read") {
            resources.format_skills_for_prompt()
        } else {
            String::new()
        };
        let test_mode = std::env::var_os("PI_TEST_MODE").is_some();

        let agent_session = build_surface_agent_session(
            &cli,
            &config,
            &self.cwd,
            &selection,
            resolved_key,
            session,
            &mut model_registry,
            &mut auth,
            &enabled_tools,
            if skills_prompt.is_empty() {
                None
            } else {
                Some(skills_prompt.as_str())
            },
            &global_dir,
            &package_dir,
            test_mode,
            &extension_flags,
            resources.extensions(),
            extension_prewarm_handle,
        )
        .await?;

        let session_handle = agent_session.session.clone();
        let result = run_selected_surface_route(
            surface_bootstrap.route_kind,
            agent_session,
            &selection,
            model_registry.get_available(),
            initial,
            messages,
            resources,
            &config,
            auth.clone(),
            self.runtime_handle.clone(),
            resource_cli,
            self.cwd.clone(),
            !cli.no_session,
            &mode,
        )
        .await;

        flush_session_autosave_on_shutdown(&session_handle, cli.no_session).await;
        result
    }
}

/// Builder for [`ApplicationKernel`].
#[derive(Default)]
pub struct ApplicationKernelBuilder {
    runtime_handle: Option<RuntimeHandle>,
    cwd: Option<PathBuf>,
    list_models: Option<ListModelsFn>,
}

impl ApplicationKernelBuilder {
    #[must_use]
    pub fn runtime_handle(mut self, runtime_handle: RuntimeHandle) -> Self {
        self.runtime_handle = Some(runtime_handle);
        self
    }

    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    #[must_use]
    pub fn list_models(mut self, list_models: ListModelsFn) -> Self {
        self.list_models = Some(list_models);
        self
    }

    /// Build the kernel once the runtime dependencies are present.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the concrete startup dependencies are
    /// missing.
    pub fn build(self) -> crate::error::Result<ApplicationKernel> {
        Ok(ApplicationKernel {
            runtime_handle: self.runtime_handle.ok_or_else(|| {
                Error::validation("missing ApplicationKernel dependency `runtime_handle`")
            })?,
            cwd: self
                .cwd
                .ok_or_else(|| Error::validation("missing ApplicationKernel dependency `cwd`"))?,
            list_models: self.list_models.ok_or_else(|| {
                Error::validation("missing ApplicationKernel dependency `list_models`")
            })?,
        })
    }
}
