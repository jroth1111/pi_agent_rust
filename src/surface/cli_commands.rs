use std::fmt::Write as _;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use anyhow::{Result, bail};
use bubbletea::{Cmd, KeyMsg, KeyType, Message as BubbleMessage, Program, quit};
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli;
use crate::config::{Config, SettingsScope};
use crate::extension_index::ExtensionIndexStore;
use crate::models::{ModelEntry, ModelRegistry};
use crate::package_manager::{
    PackageEntry, PackageManager, PackageScope, ResolvedPaths, ResolvedResource, ResourceOrigin,
};
use crate::provider::InputType;
use crate::provider_metadata::PROVIDER_METADATA;

pub async fn handle_subcommand(command: cli::Commands, cwd: &Path) -> Result<()> {
    let manager = PackageManager::new(cwd.to_path_buf());
    match command {
        cli::Commands::Install { source, local } => {
            handle_package_install(&manager, &source, local).await?;
        }
        cli::Commands::Remove { source, local } => {
            handle_package_remove(&manager, &source, local).await?;
        }
        cli::Commands::Update { source } => {
            handle_package_update(&manager, source).await?;
        }
        cli::Commands::UpdateIndex => {
            handle_update_index().await?;
        }
        cli::Commands::Search {
            query,
            tag,
            sort,
            limit,
        } => {
            handle_search(&query, tag.as_deref(), &sort, limit).await?;
        }
        cli::Commands::Info { name } => {
            handle_info(&name).await?;
        }
        cli::Commands::List => {
            handle_package_list(&manager).await?;
        }
        cli::Commands::Config { show, paths, json } => {
            handle_config(&manager, cwd, show, paths, json).await?;
        }
        cli::Commands::Doctor {
            path,
            format,
            policy,
            fix,
            only,
        } => {
            handle_doctor(
                cwd,
                path.as_deref(),
                &format,
                policy.as_deref(),
                fix,
                only.as_deref(),
            )?;
        }
        cli::Commands::Migrate { path, dry_run } => {
            handle_session_migrate(&path, dry_run)?;
        }
        cli::Commands::Account { command } => {
            handle_account_command(command)?;
        }
    }

    Ok(())
}

pub fn handle_package_list_blocking(manager: &PackageManager) -> Result<()> {
    let entries = manager.list_packages_blocking()?;
    print_package_list_entries_blocking(manager, entries, print_package_entry_blocking)
}

pub fn handle_search_blocking(
    query: &str,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
) -> Result<bool> {
    let store = ExtensionIndexStore::default_store();
    let index = store.load_or_seed()?;

    let has_cache = store.path().exists();
    if has_cache
        && index.is_stale(
            chrono::Utc::now(),
            crate::extension_index::DEFAULT_INDEX_MAX_AGE,
        )
    {
        return Ok(false);
    }

    render_search_results(&index, query, tag, sort, limit);
    Ok(true)
}

pub fn handle_info_blocking(name: &str) -> Result<()> {
    let index = ExtensionIndexStore::default_store().load_or_seed()?;
    let entry = find_index_entry_by_name_or_id(&index, name);
    let Some(entry) = entry else {
        println!("Extension \"{name}\" not found.");
        println!("Try: pi search {name}");
        return Ok(());
    };
    print_extension_info(entry);
    Ok(())
}

pub fn handle_doctor(
    cwd: &Path,
    extension_path: Option<&str>,
    format: &str,
    policy_override: Option<&str>,
    fix: bool,
    only: Option<&str>,
) -> Result<()> {
    use crate::doctor::{CheckCategory, DoctorOptions};

    let only_set = if let Some(raw) = only {
        let mut parsed = std::collections::HashSet::new();
        let mut invalid = Vec::new();
        for part in raw.split(',') {
            let name = part.trim();
            if name.is_empty() {
                continue;
            }
            match name.parse::<CheckCategory>() {
                Ok(cat) => {
                    parsed.insert(cat);
                }
                Err(_) => invalid.push(name.to_string()),
            }
        }
        if !invalid.is_empty() {
            bail!(
                "Unknown --only categories: {} (valid: config, dirs, auth, shell, sessions, extensions)",
                invalid.join(", ")
            );
        }
        if parsed.is_empty() {
            bail!(
                "--only must include at least one category (valid: config, dirs, auth, shell, sessions, extensions)"
            );
        }
        Some(parsed)
    } else {
        None
    };

    let opts = DoctorOptions {
        cwd,
        extension_path,
        policy_override,
        fix,
        only: only_set,
    };

    let report = crate::doctor::run_doctor(&opts)?;

    match format {
        "json" => {
            println!("{}", report.to_json()?);
        }
        "markdown" | "md" => {
            print!("{}", report.render_markdown());
        }
        _ => {
            print!("{}", report.render_text());
        }
    }

    if report.overall == crate::doctor::Severity::Fail {
        std::process::exit(1);
    }

    Ok(())
}

pub fn print_version() {
    println!(
        "pi {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
    );
}

pub fn list_models(registry: &ModelRegistry, pattern: Option<&str>) {
    let mut models = registry.get_available();
    if models.is_empty() {
        println!("No models available. Set API keys in environment variables.");
        return;
    }

    if let Some(pattern) = pattern {
        models = filter_models_by_pattern(models, pattern);
        if models.is_empty() {
            println!("No models matching \"{pattern}\"");
            return;
        }
    }

    models.sort_by(|a, b| {
        let provider_cmp = a.model.provider.cmp(&b.model.provider);
        if provider_cmp == std::cmp::Ordering::Equal {
            a.model.id.cmp(&b.model.id)
        } else {
            provider_cmp
        }
    });

    let rows = build_model_rows(&models);
    print_model_table(&rows);
}

pub fn list_providers() {
    let mut rows: Vec<(&str, &str, String, String, &str)> = PROVIDER_METADATA
        .iter()
        .map(|meta| {
            let display = meta.canonical_id;
            let aliases = if meta.aliases.is_empty() {
                String::new()
            } else {
                meta.aliases.join(", ")
            };
            let env_keys = meta.auth_env_keys.join(", ");
            let api = meta.routing_defaults.map_or("-", |defaults| defaults.api);
            (meta.canonical_id, display, aliases, env_keys, api)
        })
        .collect();
    rows.sort_by_key(|(id, _, _, _, _)| *id);

    let id_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(8);
    let name_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(4);
    let alias_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(7);
    let env_w = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(8);
    let api_w = rows.iter().map(|r| r.4.len()).max().unwrap_or(0).max(3);

    println!(
        "{:<id_w$}  {:<name_w$}  {:<alias_w$}  {:<env_w$}  {:<api_w$}",
        "provider", "name", "aliases", "auth env", "api",
    );
    println!(
        "{:<id_w$}  {:<name_w$}  {:<alias_w$}  {:<env_w$}  {:<api_w$}",
        "-".repeat(id_w),
        "-".repeat(name_w),
        "-".repeat(alias_w),
        "-".repeat(env_w),
        "-".repeat(api_w),
    );
    for (id, name, aliases, env_keys, api) in &rows {
        println!(
            "{id:<id_w$}  {name:<name_w$}  {aliases:<alias_w$}  {env_keys:<env_w$}  {api:<api_w$}"
        );
    }
    println!("\n{} providers available.", rows.len());
}

const fn scope_from_flag(local: bool) -> PackageScope {
    if local {
        PackageScope::Project
    } else {
        PackageScope::User
    }
}

async fn handle_package_install(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    let resolved_source = manager.resolve_install_source_alias(source);
    manager.install(&resolved_source, scope).await?;
    manager.add_package_source(&resolved_source, scope).await?;
    if resolved_source == source {
        println!("Installed {source}");
    } else {
        println!("Installed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

async fn handle_package_remove(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    let resolved_source = manager.resolve_install_source_alias(source);
    manager.remove(&resolved_source, scope).await?;
    manager
        .remove_package_source(&resolved_source, scope)
        .await?;
    if resolved_source == source {
        println!("Removed {source}");
    } else {
        println!("Removed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

async fn handle_package_update(manager: &PackageManager, source: Option<String>) -> Result<()> {
    let entries = manager.list_packages().await?;

    if let Some(source) = source {
        let resolved_source = manager.resolve_install_source_alias(&source);
        let identity = manager.package_identity(&resolved_source);
        for entry in entries {
            if manager.package_identity(&entry.source) != identity {
                continue;
            }
            manager.update_source(&entry.source, entry.scope).await?;
        }
        if resolved_source == source {
            println!("Updated {source}");
        } else {
            println!("Updated {source} (resolved to {resolved_source})");
        }
        return Ok(());
    }

    for entry in entries {
        manager.update_source(&entry.source, entry.scope).await?;
    }
    println!("Updated packages");
    Ok(())
}

async fn handle_package_list(manager: &PackageManager) -> Result<()> {
    let entries = manager.list_packages().await?;
    let (user, project) = split_package_entries(entries);

    if user.is_empty() && project.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }

    if !user.is_empty() {
        println!("User packages:");
        for entry in &user {
            print_package_entry(manager, entry).await?;
        }
    }

    if !project.is_empty() {
        if !user.is_empty() {
            println!();
        }
        println!("Project packages:");
        for entry in &project {
            print_package_entry(manager, entry).await?;
        }
    }

    Ok(())
}

fn split_package_entries(entries: Vec<PackageEntry>) -> (Vec<PackageEntry>, Vec<PackageEntry>) {
    let mut user = Vec::new();
    let mut project = Vec::new();
    for entry in entries {
        match entry.scope {
            PackageScope::User => user.push(entry),
            PackageScope::Project | PackageScope::Temporary => project.push(entry),
        }
    }
    (user, project)
}

fn print_package_list_entries_blocking<F>(
    manager: &PackageManager,
    entries: Vec<PackageEntry>,
    mut print_entry: F,
) -> Result<()>
where
    F: FnMut(&PackageManager, &PackageEntry) -> Result<()>,
{
    let (user, project) = split_package_entries(entries);

    if user.is_empty() && project.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }

    if !user.is_empty() {
        println!("User packages:");
        for entry in &user {
            print_entry(manager, entry)?;
        }
    }

    if !project.is_empty() {
        if !user.is_empty() {
            println!();
        }
        println!("Project packages:");
        for entry in &project {
            print_entry(manager, entry)?;
        }
    }

    Ok(())
}

async fn handle_update_index() -> Result<()> {
    let store = ExtensionIndexStore::default_store();
    let client = crate::http::client::Client::new();
    let (_, stats) = store.refresh_best_effort(&client).await?;

    if !stats.refreshed {
        println!(
            "Extension index refresh skipped: remote sources unavailable; using existing seed/cache."
        );
        return Ok(());
    }

    println!(
        "Extension index refreshed: {} merged entries (npm: {}, github: {}) at {}",
        stats.merged_entries,
        stats.npm_entries,
        stats.github_entries,
        store.path().display()
    );
    Ok(())
}

async fn handle_search(query: &str, tag: Option<&str>, sort: &str, limit: usize) -> Result<()> {
    let store = ExtensionIndexStore::default_store();
    let mut index = store.load_or_seed()?;
    let has_cache = store.path().exists();
    if has_cache
        && index.is_stale(
            chrono::Utc::now(),
            crate::extension_index::DEFAULT_INDEX_MAX_AGE,
        )
    {
        println!("Refreshing extension index...");
        let client = crate::http::client::Client::new();
        match store.refresh_best_effort(&client).await {
            Ok((refreshed, _)) => index = refreshed,
            Err(_) => {
                println!(
                    "Warning: Could not refresh index (network unavailable). Using cached results."
                );
            }
        }
    }

    render_search_results(&index, query, tag, sort, limit);
    Ok(())
}

fn render_search_results(
    index: &crate::extension_index::ExtensionIndex,
    query: &str,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
) {
    let hits = collect_search_hits(index, tag, sort, limit, query);
    if hits.is_empty() {
        println!("No extensions found for \"{query}\".");
        return;
    }

    print_search_results(&hits);
}

fn collect_search_hits(
    index: &crate::extension_index::ExtensionIndex,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
    query: &str,
) -> Vec<crate::extension_index::ExtensionSearchHit> {
    let mut hits = index.search(query, limit);

    if let Some(tag_filter) = tag {
        let tag_lower = tag_filter.to_ascii_lowercase();
        hits.retain(|hit| {
            hit.entry
                .tags
                .iter()
                .any(|t| t.to_ascii_lowercase() == tag_lower)
        });
    }

    if sort == "name" {
        hits.sort_by(|a, b| {
            a.entry
                .name
                .to_ascii_lowercase()
                .cmp(&b.entry.name.to_ascii_lowercase())
        });
    }

    hits
}

#[allow(clippy::uninlined_format_args)]
fn print_search_results(hits: &[crate::extension_index::ExtensionSearchHit]) {
    let name_w = hits
        .iter()
        .map(|h| h.entry.name.len())
        .max()
        .unwrap_or(0)
        .max(4);
    let desc_w = hits
        .iter()
        .map(|h| h.entry.description.as_deref().unwrap_or("").len().min(50))
        .max()
        .unwrap_or(0)
        .max(11);
    let tags_w = hits
        .iter()
        .map(|h| h.entry.tags.join(", ").len().min(30))
        .max()
        .unwrap_or(0)
        .max(4);
    let source_w = 6;

    println!(
        "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
        "Name", "Description", "Tags", "Source"
    );
    println!(
        "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
        "-".repeat(name_w),
        "-".repeat(desc_w),
        "-".repeat(tags_w),
        "-".repeat(source_w)
    );

    for hit in hits {
        let desc = hit.entry.description.as_deref().unwrap_or("");
        let desc_truncated = if desc.chars().count() > 50 {
            let truncated: String = desc.chars().take(47).collect();
            format!("{truncated}...")
        } else {
            desc.to_string()
        };
        let tags_joined = hit.entry.tags.join(", ");
        let tags_truncated = if tags_joined.chars().count() > 30 {
            let truncated: String = tags_joined.chars().take(27).collect();
            format!("{truncated}...")
        } else {
            tags_joined
        };
        let source_label = match &hit.entry.source {
            Some(crate::extension_index::ExtensionIndexSource::Npm { .. }) => "npm",
            Some(crate::extension_index::ExtensionIndexSource::Git { .. }) => "git",
            Some(crate::extension_index::ExtensionIndexSource::Url { .. }) => "url",
            None => "-",
        };
        println!(
            "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
            hit.entry.name, desc_truncated, tags_truncated, source_label
        );
    }

    let count = hits.len();
    let noun = if count == 1 {
        "extension"
    } else {
        "extensions"
    };
    println!("\n  {count} {noun} found. Install with: pi install <name>");
}

async fn handle_info(name: &str) -> Result<()> {
    handle_info_blocking(name)
}

fn find_index_entry_by_name_or_id<'a>(
    index: &'a crate::extension_index::ExtensionIndex,
    name: &str,
) -> Option<&'a crate::extension_index::ExtensionIndexEntry> {
    index
        .entries
        .iter()
        .find(|e| e.id.eq_ignore_ascii_case(name) || e.name.eq_ignore_ascii_case(name))
        .or_else(|| {
            let hits = index.search(name, 1);
            hits.into_iter()
                .next()
                .map(|h| h.entry)
                .and_then(|matched| index.entries.iter().find(|e| e.id == matched.id))
        })
}

fn print_extension_info(entry: &crate::extension_index::ExtensionIndexEntry) {
    let width = 60;
    let bar = "─".repeat(width);

    println!("  ┌{bar}┐");
    let title = &entry.name;
    let padding = width.saturating_sub(title.len() + 1);
    println!("  │ {title}{:padding$}│", "");

    if entry.id != entry.name {
        let id_line = format!("id: {}", entry.id);
        let padding = width.saturating_sub(id_line.len() + 1);
        println!("  │ {id_line}{:padding$}│", "");
    }

    if let Some(desc) = &entry.description {
        println!("  │{:width$}│", "");
        for line in wrap_text(desc, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    }

    println!("  ├{bar}┤");

    if !entry.tags.is_empty() {
        let tags_line = format!("Tags: {}", entry.tags.join(", "));
        let padding = width.saturating_sub(tags_line.len() + 1);
        println!("  │ {tags_line}{:padding$}│", "");
    }

    if let Some(license) = &entry.license {
        let lic_line = format!("License: {license}");
        let padding = width.saturating_sub(lic_line.len() + 1);
        println!("  │ {lic_line}{:padding$}│", "");
    }

    if let Some(source) = &entry.source {
        let source_line = match source {
            crate::extension_index::ExtensionIndexSource::Npm {
                package, version, ..
            } => {
                let ver = version.as_deref().unwrap_or("latest");
                format!("Source: npm:{package}@{ver}")
            }
            crate::extension_index::ExtensionIndexSource::Git { repo, path, .. } => {
                let suffix = path.as_deref().map_or(String::new(), |p| format!(" ({p})"));
                format!("Source: git:{repo}{suffix}")
            }
            crate::extension_index::ExtensionIndexSource::Url { url } => {
                format!("Source: {url}")
            }
        };
        for line in wrap_text(&source_line, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    }

    println!("  ├{bar}┤");
    if let Some(install_source) = &entry.install_source {
        let install_line = format!("Install: pi install {install_source}");
        for line in wrap_text(&install_line, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    } else {
        let hint = "Install source not available";
        let padding = width.saturating_sub(hint.len() + 1);
        println!("  │ {hint}{:padding$}│", "");
    }

    println!("  └{bar}┘");
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current = word.to_string();
            } else if current.len() + 1 + word.len() <= max_width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

async fn print_package_entry(manager: &PackageManager, entry: &PackageEntry) -> Result<()> {
    let display = if entry.filter.is_some() {
        format!("{} (filtered)", entry.source)
    } else {
        entry.source.clone()
    };
    println!("  {display}");
    if let Some(path) = manager.installed_path(&entry.source, entry.scope).await? {
        println!("    {}", path.display());
    }
    Ok(())
}

fn print_package_entry_blocking(manager: &PackageManager, entry: &PackageEntry) -> Result<()> {
    let display = if entry.filter.is_some() {
        format!("{} (filtered)", entry.source)
    } else {
        entry.source.clone()
    };
    println!("  {display}");
    if let Some(path) = manager.installed_path_blocking(&entry.source, entry.scope)? {
        println!("    {}", path.display());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ConfigResourceKind {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ConfigResourceKind {
    const ALL: [Self; 4] = [Self::Extensions, Self::Skills, Self::Prompts, Self::Themes];

    const fn field_name(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Extensions => "extension",
            Self::Skills => "skill",
            Self::Prompts => "prompt",
            Self::Themes => "theme",
        }
    }

    const fn order(self) -> usize {
        match self {
            Self::Extensions => 0,
            Self::Skills => 1,
            Self::Prompts => 2,
            Self::Themes => 3,
        }
    }
}

#[derive(Debug, Clone)]
struct ConfigResourceState {
    kind: ConfigResourceKind,
    path: String,
    enabled: bool,
}

#[derive(Debug, Clone)]
struct ConfigPackageState {
    scope: SettingsScope,
    source: String,
    resources: Vec<ConfigResourceState>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPathsReport {
    global: String,
    project: String,
    auth: String,
    sessions: String,
    packages: String,
    extension_index: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigResourceReport {
    kind: String,
    path: String,
    enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPackageReport {
    scope: String,
    source: String,
    resources: Vec<ConfigResourceReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigReport {
    paths: ConfigPathsReport,
    precedence: Vec<String>,
    config_valid: bool,
    config_error: Option<String>,
    packages: Vec<ConfigPackageReport>,
}

#[derive(Debug, Clone, Default)]
struct PackageFilterState {
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PackageFilterState {
    fn set_kind(&mut self, kind: ConfigResourceKind, values: Vec<String>) {
        match kind {
            ConfigResourceKind::Extensions => self.extensions = Some(values),
            ConfigResourceKind::Skills => self.skills = Some(values),
            ConfigResourceKind::Prompts => self.prompts = Some(values),
            ConfigResourceKind::Themes => self.themes = Some(values),
        }
    }

    const fn values_for_kind(&self, kind: ConfigResourceKind) -> Option<&Vec<String>> {
        match kind {
            ConfigResourceKind::Extensions => self.extensions.as_ref(),
            ConfigResourceKind::Skills => self.skills.as_ref(),
            ConfigResourceKind::Prompts => self.prompts.as_ref(),
            ConfigResourceKind::Themes => self.themes.as_ref(),
        }
    }

    const fn has_any_field(&self) -> bool {
        self.extensions.is_some()
            || self.skills.is_some()
            || self.prompts.is_some()
            || self.themes.is_some()
    }
}

#[derive(Debug, Clone)]
struct ConfigUiResult {
    save_requested: bool,
    packages: Vec<ConfigPackageState>,
}

#[derive(bubbletea::Model)]
struct ConfigUiApp {
    packages: Vec<ConfigPackageState>,
    selected: usize,
    settings_summary: String,
    status: String,
    result_slot: Arc<StdMutex<Option<ConfigUiResult>>>,
}

impl ConfigUiApp {
    fn new(
        packages: Vec<ConfigPackageState>,
        settings_summary: String,
        result_slot: Arc<StdMutex<Option<ConfigUiResult>>>,
    ) -> Self {
        let status = if packages.iter().any(|pkg| !pkg.resources.is_empty()) {
            String::new()
        } else {
            "No package resources discovered. Press Enter to exit.".to_string()
        };

        Self {
            packages,
            selected: 0,
            settings_summary,
            status,
            result_slot,
        }
    }

    fn selectable_count(&self) -> usize {
        self.packages.iter().map(|pkg| pkg.resources.len()).sum()
    }

    fn selected_coords(&self) -> Option<(usize, usize)> {
        let mut cursor = 0usize;
        for (pkg_idx, pkg) in self.packages.iter().enumerate() {
            for (res_idx, _) in pkg.resources.iter().enumerate() {
                if cursor == self.selected {
                    return Some((pkg_idx, res_idx));
                }
                cursor = cursor.saturating_add(1);
            }
        }
        None
    }

    fn move_selection(&mut self, delta: isize) {
        let total = self.selectable_count();
        if total == 0 {
            self.selected = 0;
            return;
        }

        let max_index = total.saturating_sub(1);
        let step = delta.unsigned_abs();
        if delta.is_negative() {
            self.selected = self.selected.saturating_sub(step);
        } else {
            self.selected = self.selected.saturating_add(step).min(max_index);
        }
    }

    fn toggle_selected(&mut self) {
        if let Some((pkg_idx, res_idx)) = self.selected_coords() {
            if let Some(resource) = self
                .packages
                .get_mut(pkg_idx)
                .and_then(|pkg| pkg.resources.get_mut(res_idx))
            {
                resource.enabled = !resource.enabled;
            }
        }
    }

    fn finish(&self, save_requested: bool) -> Cmd {
        if let Ok(mut slot) = self.result_slot.lock() {
            *slot = Some(ConfigUiResult {
                save_requested,
                packages: self.packages.clone(),
            });
        }
        quit()
    }

    #[allow(clippy::missing_const_for_fn, clippy::unused_self)]
    fn init(&self) -> Option<Cmd> {
        None
    }

    #[allow(clippy::needless_pass_by_value)]
    fn update(&mut self, msg: BubbleMessage) -> Option<Cmd> {
        if let Some(key) = msg.downcast_ref::<KeyMsg>() {
            match key.key_type {
                KeyType::Up => self.move_selection(-1),
                KeyType::Down => self.move_selection(1),
                KeyType::Runes if key.runes == ['k'] => self.move_selection(-1),
                KeyType::Runes if key.runes == ['j'] => self.move_selection(1),
                KeyType::Space => self.toggle_selected(),
                KeyType::Enter => return Some(self.finish(true)),
                KeyType::Esc | KeyType::CtrlC => return Some(self.finish(false)),
                KeyType::Runes if key.runes == ['q'] => return Some(self.finish(false)),
                _ => {}
            }
        }
        None
    }

    fn view(&self) -> String {
        let mut out = String::new();
        out.push_str("Pi Config UI\n");
        let _ = writeln!(out, "{}", self.settings_summary);
        out.push_str("Keys: ↑/↓ (or j/k) move, Space toggle, Enter save, q cancel\n\n");

        let mut cursor = 0usize;
        for package in &self.packages {
            let _ = writeln!(
                out,
                "{} package: {}",
                scope_label(package.scope),
                package.source
            );

            if package.resources.is_empty() {
                out.push_str("    (no discovered resources)\n");
                continue;
            }

            for resource in &package.resources {
                let selected = cursor == self.selected;
                let marker = if resource.enabled { "x" } else { " " };
                let prefix = if selected { ">" } else { " " };
                let _ = writeln!(
                    out,
                    "{} [{}] {:<10} {}",
                    prefix,
                    marker,
                    resource.kind.label(),
                    resource.path
                );
                cursor = cursor.saturating_add(1);
            }

            out.push('\n');
        }

        if !self.status.is_empty() {
            let _ = writeln!(out, "{}", self.status);
        }

        out
    }
}

const fn scope_label(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "Global",
        SettingsScope::Project => "Project",
    }
}

const fn scope_key(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "global",
        SettingsScope::Project => "project",
    }
}

const fn settings_scope_from_package_scope(scope: PackageScope) -> Option<SettingsScope> {
    match scope {
        PackageScope::User => Some(SettingsScope::Global),
        PackageScope::Project => Some(SettingsScope::Project),
        PackageScope::Temporary => None,
    }
}

fn package_lookup_key(scope: SettingsScope, source: &str) -> String {
    format!("{}::{source}", scope_key(scope))
}

fn normalize_path_for_display(path: &Path, base_dir: Option<&Path>) -> String {
    let rel = base_dir
        .and_then(|base| path.strip_prefix(base).ok())
        .unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

fn normalize_filter_entry(path: &str) -> String {
    path.replace('\\', "/")
}

fn merge_resolved_resources(
    kind: ConfigResourceKind,
    resources: &[ResolvedResource],
    packages: &mut Vec<ConfigPackageState>,
    lookup: &mut std::collections::HashMap<String, usize>,
) {
    for resource in resources {
        if resource.metadata.origin != ResourceOrigin::Package {
            continue;
        }

        let Some(scope) = settings_scope_from_package_scope(resource.metadata.scope) else {
            continue;
        };

        let key = package_lookup_key(scope, &resource.metadata.source);
        let idx = lookup.get(&key).copied().unwrap_or_else(|| {
            let idx = packages.len();
            packages.push(ConfigPackageState {
                scope,
                source: resource.metadata.source.clone(),
                resources: Vec::new(),
            });
            lookup.insert(key, idx);
            idx
        });

        let path =
            normalize_path_for_display(&resource.path, resource.metadata.base_dir.as_deref());
        packages[idx].resources.push(ConfigResourceState {
            kind,
            path,
            enabled: resource.enabled,
        });
    }
}

fn sort_and_dedupe_package_resources(packages: &mut [ConfigPackageState]) {
    for package in packages {
        package.resources.sort_by(|a, b| {
            (a.kind.order(), a.path.as_str()).cmp(&(b.kind.order(), b.path.as_str()))
        });

        let mut deduped: Vec<ConfigResourceState> = Vec::new();
        for resource in std::mem::take(&mut package.resources) {
            if let Some(existing) = deduped
                .iter_mut()
                .find(|r| r.kind == resource.kind && r.path == resource.path)
            {
                existing.enabled = existing.enabled || resource.enabled;
            } else {
                deduped.push(resource);
            }
        }
        package.resources = deduped;
    }
}

async fn collect_config_packages(manager: &PackageManager) -> Vec<ConfigPackageState> {
    let mut packages = Vec::new();
    let mut lookup = std::collections::HashMap::<String, usize>::new();

    let entries = manager.list_packages().await.unwrap_or_default();
    for entry in entries {
        let Some(scope) = settings_scope_from_package_scope(entry.scope) else {
            continue;
        };
        let key = package_lookup_key(scope, &entry.source);
        if lookup.contains_key(&key) {
            continue;
        }
        lookup.insert(key, packages.len());
        packages.push(ConfigPackageState {
            scope,
            source: entry.source,
            resources: Vec::new(),
        });
    }

    match manager.resolve().await {
        Ok(ResolvedPaths {
            extensions,
            skills,
            prompts,
            themes,
        }) => {
            merge_resolved_resources(
                ConfigResourceKind::Extensions,
                &extensions,
                &mut packages,
                &mut lookup,
            );
            merge_resolved_resources(
                ConfigResourceKind::Skills,
                &skills,
                &mut packages,
                &mut lookup,
            );
            merge_resolved_resources(
                ConfigResourceKind::Prompts,
                &prompts,
                &mut packages,
                &mut lookup,
            );
            merge_resolved_resources(
                ConfigResourceKind::Themes,
                &themes,
                &mut packages,
                &mut lookup,
            );
        }
        Err(err) => {
            eprintln!("Warning: failed to resolve package resources for config UI: {err}");
        }
    }

    sort_and_dedupe_package_resources(&mut packages);
    packages
}

fn build_config_report(cwd: &Path, packages: &[ConfigPackageState]) -> ConfigReport {
    let config_path = std::env::var("PI_CONFIG_PATH")
        .ok()
        .map_or_else(|| Config::global_dir().join("settings.json"), PathBuf::from);
    let project_path = cwd.join(Config::project_dir()).join("settings.json");

    let (config_valid, config_error) = match Config::load() {
        Ok(_) => (true, None),
        Err(err) => (false, Some(err.to_string())),
    };

    let packages = packages
        .iter()
        .map(|package| ConfigPackageReport {
            scope: scope_key(package.scope).to_string(),
            source: package.source.clone(),
            resources: package
                .resources
                .iter()
                .map(|resource| ConfigResourceReport {
                    kind: resource.kind.field_name().to_string(),
                    path: resource.path.clone(),
                    enabled: resource.enabled,
                })
                .collect(),
        })
        .collect::<Vec<_>>();

    ConfigReport {
        paths: ConfigPathsReport {
            global: config_path.display().to_string(),
            project: project_path.display().to_string(),
            auth: Config::auth_path().display().to_string(),
            sessions: Config::sessions_dir().display().to_string(),
            packages: Config::package_dir().display().to_string(),
            extension_index: Config::extension_index_path().display().to_string(),
        },
        precedence: vec![
            "CLI flags".to_string(),
            "Environment variables".to_string(),
            format!("Project settings ({})", project_path.display()),
            format!("Global settings ({})", config_path.display()),
            "Built-in defaults".to_string(),
        ],
        config_valid,
        config_error,
        packages,
    }
}

fn print_config_report(report: &ConfigReport, include_packages: bool) {
    println!("Settings paths:");
    println!("  Global:  {}", report.paths.global);
    println!("  Project: {}", report.paths.project);
    println!();
    println!("Other paths:");
    println!("  Auth:     {}", report.paths.auth);
    println!("  Sessions: {}", report.paths.sessions);
    println!("  Packages: {}", report.paths.packages);
    println!("  ExtIndex: {}", report.paths.extension_index);
    println!();
    println!("Settings precedence:");
    for (idx, entry) in report.precedence.iter().enumerate() {
        println!("  {}) {}", idx + 1, entry);
    }
    println!();

    if report.config_valid {
        println!("Current configuration is valid.");
    } else if let Some(error) = &report.config_error {
        println!("Configuration Error: {error}");
    }

    if !include_packages {
        return;
    }

    println!();
    println!("Package resources:");
    if report.packages.is_empty() {
        println!("  (no configured packages)");
        return;
    }

    for package in &report.packages {
        println!("  [{}] {}", package.scope, package.source);
        if package.resources.is_empty() {
            println!("    (no discovered resources)");
            continue;
        }
        for resource in &package.resources {
            let marker = if resource.enabled { "x" } else { " " };
            println!("    [{}] {:<10} {}", marker, resource.kind, resource.path);
        }
    }
}

fn format_settings_summary(config: &Config) -> String {
    let provider = config.default_provider.as_deref().unwrap_or("(default)");
    let model = config.default_model.as_deref().unwrap_or("(default)");
    let thinking = config
        .default_thinking_level
        .as_deref()
        .unwrap_or("(default)");
    format!("provider={provider}  model={model}  thinking={thinking}")
}

fn run_config_tui(
    packages: Vec<ConfigPackageState>,
    settings_summary: String,
) -> Result<Option<Vec<ConfigPackageState>>> {
    let result_slot = Arc::new(StdMutex::new(None));
    let app = ConfigUiApp::new(packages, settings_summary, Arc::clone(&result_slot));
    Program::new(app).with_alt_screen().run()?;

    let result = result_slot.lock().ok().and_then(|guard| guard.clone());
    match result {
        Some(result) if result.save_requested => Ok(Some(result.packages)),
        _ => Ok(None),
    }
}

fn load_settings_json_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }

    let content = std::fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(json!({}));
    }

    let value: Value = serde_json::from_str(&content)?;
    if value.is_object() {
        Ok(value)
    } else {
        Ok(json!({}))
    }
}

fn extract_package_source(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string).or_else(|| {
        value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn persist_package_toggles(cwd: &Path, packages: &[ConfigPackageState]) -> Result<()> {
    let global_dir = Config::global_dir();
    persist_package_toggles_with_roots(cwd, &global_dir, packages)
}

#[allow(clippy::too_many_lines)]
fn persist_package_toggles_with_roots(
    cwd: &Path,
    global_dir: &Path,
    packages: &[ConfigPackageState],
) -> Result<()> {
    let mut updates_by_scope: std::collections::HashMap<
        SettingsScope,
        std::collections::HashMap<String, PackageFilterState>,
    > = std::collections::HashMap::new();

    for package in packages {
        if package.resources.is_empty() {
            continue;
        }

        let mut state = PackageFilterState::default();
        for kind in ConfigResourceKind::ALL {
            let kind_resources = package
                .resources
                .iter()
                .filter(|resource| resource.kind == kind)
                .collect::<Vec<_>>();
            if kind_resources.is_empty() {
                continue;
            }

            let mut enabled = kind_resources
                .iter()
                .filter(|resource| resource.enabled)
                .map(|resource| normalize_filter_entry(&resource.path))
                .collect::<Vec<_>>();
            enabled.sort();
            enabled.dedup();
            state.set_kind(kind, enabled);
        }

        if !state.has_any_field() {
            continue;
        }

        updates_by_scope
            .entry(package.scope)
            .or_default()
            .insert(package.source.clone(), state);
    }

    for scope in [SettingsScope::Global, SettingsScope::Project] {
        let Some(scope_updates) = updates_by_scope.get(&scope) else {
            continue;
        };

        let settings_path = Config::settings_path_with_roots(scope, global_dir, cwd);
        let mut settings = load_settings_json_object(&settings_path)?;
        if !settings.is_object() {
            settings = json!({});
        }

        let packages_array = settings
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("settings should be an object"))?
            .entry("packages".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !packages_array.is_array() {
            *packages_array = Value::Array(Vec::new());
        }

        let package_entries = packages_array
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("packages should be an array"))?;

        let mut updated_sources = std::collections::HashSet::new();
        for entry in package_entries.iter_mut() {
            let Some(source) = extract_package_source(entry) else {
                continue;
            };
            let Some(filter_state) = scope_updates.get(&source) else {
                continue;
            };

            let mut obj = entry
                .as_object()
                .cloned()
                .unwrap_or_else(serde_json::Map::new);
            obj.insert("source".to_string(), Value::String(source.clone()));
            for kind in ConfigResourceKind::ALL {
                if let Some(values) = filter_state.values_for_kind(kind) {
                    let arr = values
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>();
                    obj.insert(kind.field_name().to_string(), Value::Array(arr));
                }
            }
            *entry = Value::Object(obj);
            updated_sources.insert(source);
        }

        for (source, filter_state) in scope_updates {
            if updated_sources.contains(source) {
                continue;
            }

            let mut obj = serde_json::Map::new();
            obj.insert("source".to_string(), Value::String(source.clone()));
            for kind in ConfigResourceKind::ALL {
                if let Some(values) = filter_state.values_for_kind(kind) {
                    let arr = values
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>();
                    obj.insert(kind.field_name().to_string(), Value::Array(arr));
                }
            }
            package_entries.push(Value::Object(obj));
        }

        let patch = json!({ "packages": package_entries.clone() });
        Config::patch_settings_with_roots(scope, global_dir, cwd, patch)?;
    }

    Ok(())
}

async fn handle_config(
    manager: &PackageManager,
    cwd: &Path,
    show: bool,
    paths: bool,
    json_output: bool,
) -> Result<()> {
    if json_output && (show || paths) {
        bail!("`pi config --json` cannot be combined with --show/--paths");
    }

    let packages = collect_config_packages(manager).await;
    let report = build_config_report(cwd, &packages);

    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let interactive_requested = !show && !paths;
    let has_tty = io::stdin().is_terminal() && io::stdout().is_terminal();

    if interactive_requested && has_tty {
        let config = Config::load().unwrap_or_default();
        let settings_summary = format_settings_summary(&config);
        if let Some(updated) = run_config_tui(packages, settings_summary)? {
            persist_package_toggles(cwd, &updated)?;
            println!("Saved package resource toggles.");
        } else {
            println!("No changes saved.");
        }
        return Ok(());
    }

    print_config_report(&report, show);
    Ok(())
}

fn collect_session_jsonl_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }

    files.sort();
    Ok(files)
}

fn handle_session_migrate(path: &str, dry_run: bool) -> Result<()> {
    let path = std::path::Path::new(path);
    if !path.exists() {
        bail!("Path does not exist: {}", path.display());
    }

    let jsonl_files: Vec<std::path::PathBuf> = if path.is_dir() {
        let files = collect_session_jsonl_files(path)?;
        if files.is_empty() {
            bail!("No .jsonl session files found in {}", path.display());
        }
        files
    } else {
        vec![path.to_path_buf()]
    };

    let mut migrated = 0u64;
    let mut errors = 0u64;

    for jsonl_path in &jsonl_files {
        if dry_run {
            match crate::session::migrate_dry_run(jsonl_path) {
                Ok(verification) => {
                    let status = if verification.entry_count_match
                        && verification.hash_chain_match
                        && verification.index_consistent
                    {
                        "OK"
                    } else {
                        "MISMATCH"
                    };
                    println!(
                        "[dry-run] {}: {} (entries_match={}, hash_match={}, index_ok={})",
                        jsonl_path.display(),
                        status,
                        verification.entry_count_match,
                        verification.hash_chain_match,
                        verification.index_consistent,
                    );
                    migrated += 1;
                }
                Err(e) => {
                    eprintln!("[dry-run] {}: ERROR: {e}", jsonl_path.display());
                    errors += 1;
                }
            }
        } else {
            let correlation_id = uuid::Uuid::new_v4().to_string();
            match crate::session::migrate_jsonl_to_v2(jsonl_path, &correlation_id) {
                Ok(event) => {
                    println!(
                        "[migrated] {}: migration_id={}, entries_match={}, hash_match={}, index_ok={}",
                        jsonl_path.display(),
                        event.migration_id,
                        event.verification.entry_count_match,
                        event.verification.hash_chain_match,
                        event.verification.index_consistent,
                    );
                    migrated += 1;
                }
                Err(e) => {
                    eprintln!("[error] {}: {e}", jsonl_path.display());
                    errors += 1;
                }
            }
        }
    }

    println!(
        "\nSession migration complete: {migrated} succeeded, {errors} failed (dry_run={dry_run})"
    );
    if errors > 0 {
        bail!("{errors} session(s) failed migration");
    }
    Ok(())
}

fn handle_account_command(command: cli::AccountCommands) -> Result<()> {
    use cli::AccountCommands;

    let auth_path = crate::config::Config::auth_path();

    fn parse_revocation_reason(reason: Option<String>) -> crate::auth::RevocationReason {
        match reason
            .as_deref()
            .map_or("user_requested", str::trim)
            .to_ascii_lowercase()
            .as_str()
        {
            "compromised" => crate::auth::RevocationReason::Compromised,
            "auth_failure" | "auth-failure" => crate::auth::RevocationReason::AuthFailure,
            "provider_invalidated" | "provider-invalidated" => {
                crate::auth::RevocationReason::ProviderInvalidated
            }
            "account_removed" | "account-removed" => crate::auth::RevocationReason::AccountRemoved,
            _ => crate::auth::RevocationReason::UserRequested,
        }
    }

    match command {
        AccountCommands::List { provider } => {
            let auth = crate::auth::AuthStorage::load(auth_path)?;
            let provider = provider.unwrap_or_else(|| "all".to_string());
            let output = auth.format_accounts_status(&provider);
            println!("{output}");
        }
        AccountCommands::Use { provider, account } => {
            let mut auth = crate::auth::AuthStorage::load(auth_path)?;
            auth.set_active_account(&provider, &account)?;
            auth.save()?;
            println!("Switched to account '{account}' for {provider}");
        }
        AccountCommands::Remove { provider, account } => {
            let mut auth = crate::auth::AuthStorage::load(auth_path)?;
            let accounts = auth.list_oauth_accounts(&provider);
            if accounts.iter().any(|a| a.id == account) {
                auth.remove_oauth_account(&provider, &account);
                auth.save()?;
                println!("Removed account '{account}' from {provider}");
            } else {
                bail!("Account '{account}' not found in {provider}");
            }
        }
        AccountCommands::Logout { provider } => {
            let mut auth = crate::auth::AuthStorage::load(auth_path)?;
            if let Some(provider) = provider {
                auth.remove(&provider);
                auth.save()?;
                println!("Logged out from {provider}");
            } else {
                let providers = auth.provider_names();
                for p in providers {
                    auth.remove(&p);
                }
                auth.save()?;
                println!("Logged out from all providers");
            }
        }
        AccountCommands::Status { provider, json } => {
            let auth = crate::auth::AuthStorage::load(auth_path)?;
            if json {
                if let Some(p) = provider {
                    let accounts = auth.list_oauth_accounts(&p);
                    let payload = serde_json::json!({
                        "provider": p,
                        "accounts": accounts,
                        "revocations": auth.revocation_records(),
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    let providers = auth.provider_names();
                    let mut provider_payload = serde_json::Map::new();
                    for p in providers {
                        provider_payload.insert(
                            p.clone(),
                            serde_json::json!({
                                "accounts": auth.list_oauth_accounts(&p),
                                "recovery": auth.recovery_state_summary(&p),
                            }),
                        );
                    }
                    let payload = serde_json::json!({
                        "providers": provider_payload,
                        "revocations": auth.revocation_records(),
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
            } else if let Some(p) = provider {
                let output = auth.format_accounts_status(&p);
                println!("{output}");
            } else {
                let providers = auth.provider_names();
                if providers.is_empty() {
                    println!("No providers configured.");
                } else {
                    for p in providers {
                        println!("Provider: {p}");
                        let output = auth.format_accounts_status(&p);
                        println!("{output}");
                    }
                }
            }
        }
        AccountCommands::Cleanup { dry_run } => {
            let mut auth = crate::auth::AuthStorage::load(auth_path)?;
            if dry_run {
                let report = auth.cleanup_tokens_preview();
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let report = auth.cleanup_tokens();
                auth.save()?;
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
        }
        AccountCommands::Revoke {
            provider,
            account,
            reason,
        } => {
            let mut auth = crate::auth::AuthStorage::load(auth_path)?;
            let reason = parse_revocation_reason(reason);
            auth.revoke_oauth_account_token(&provider, &account, reason)?;
            auth.save()?;
            println!("Revoked account '{account}' for {provider} ({reason})");
        }
        AccountCommands::Recovery {
            provider,
            account,
            json,
        } => {
            let auth = crate::auth::AuthStorage::load(auth_path)?;
            let mut rows = auth.recovery_state_summary(&provider);
            if let Some(account_id) = account.as_deref() {
                rows.retain(|(id, _)| id == account_id);
            }
            if json {
                let payload = serde_json::json!({
                    "provider": provider,
                    "recovery": rows,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else if rows.is_empty() {
                println!("No recovery state found for provider '{provider}'.");
            } else {
                for (account_id, state) in rows {
                    println!(
                        "{}: {}",
                        account_id,
                        state.as_ref().map_or_else(
                            || "none".to_string(),
                            crate::auth::RecoveryState::description
                        )
                    );
                }
            }
        }
    }

    Ok(())
}

fn filter_models_by_pattern(models: Vec<ModelEntry>, pattern: &str) -> Vec<ModelEntry> {
    models
        .into_iter()
        .filter(|entry| {
            fuzzy_match(
                pattern,
                &format!("{} {}", entry.model.provider, entry.model.id),
            )
        })
        .collect()
}

fn build_model_rows(
    models: &[ModelEntry],
) -> Vec<(String, String, String, String, String, String)> {
    models
        .iter()
        .map(|entry| {
            let provider = entry.model.provider.clone();
            let model = entry.model.id.clone();
            let context = format_token_count(entry.model.context_window);
            let max_out = format_token_count(entry.model.max_tokens);
            let thinking = if entry.model.reasoning { "yes" } else { "no" }.to_string();
            let images = if entry.model.input.contains(&InputType::Image) {
                "yes"
            } else {
                "no"
            }
            .to_string();
            (provider, model, context, max_out, thinking, images)
        })
        .collect()
}

fn print_model_table(rows: &[(String, String, String, String, String, String)]) {
    let headers = (
        "provider", "model", "context", "max-out", "thinking", "images",
    );

    let provider_w = rows
        .iter()
        .map(|r| r.0.len())
        .max()
        .unwrap_or(0)
        .max(headers.0.len());
    let model_w = rows
        .iter()
        .map(|r| r.1.len())
        .max()
        .unwrap_or(0)
        .max(headers.1.len());
    let context_w = rows
        .iter()
        .map(|r| r.2.len())
        .max()
        .unwrap_or(0)
        .max(headers.2.len());
    let max_out_w = rows
        .iter()
        .map(|r| r.3.len())
        .max()
        .unwrap_or(0)
        .max(headers.3.len());
    let thinking_w = rows
        .iter()
        .map(|r| r.4.len())
        .max()
        .unwrap_or(0)
        .max(headers.4.len());
    let images_w = rows
        .iter()
        .map(|r| r.5.len())
        .max()
        .unwrap_or(0)
        .max(headers.5.len());

    let (provider, model, context, max_out, thinking, images) = headers;
    println!(
        "{provider:<provider_w$}  {model:<model_w$}  {context:<context_w$}  {max_out:<max_out_w$}  {thinking:<thinking_w$}  {images:<images_w$}"
    );

    for (provider, model, context, max_out, thinking, images) in rows {
        println!(
            "{provider:<provider_w$}  {model:<model_w$}  {context:<context_w$}  {max_out:<max_out_w$}  {thinking:<thinking_w$}  {images:<images_w$}"
        );
    }
}

fn format_token_count(count: u32) -> String {
    if count >= 1_000_000 {
        let millions = f64::from(count) / 1_000_000.0;
        if millions.fract() == 0.0 {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        }
    } else if count >= 1_000 {
        let thousands = f64::from(count) / 1_000.0;
        if thousands.fract() == 0.0 {
            format!("{thousands:.0}K")
        } else {
            format!("{thousands:.1}K")
        }
    } else {
        count.to_string()
    }
}

fn fuzzy_match(pattern: &str, value: &str) -> bool {
    let needle_str = pattern.to_lowercase();
    let haystack_str = value.to_lowercase();
    let mut needle = needle_str.chars().filter(|c| !c.is_whitespace());
    let mut haystack = haystack_str.chars();
    for ch in needle.by_ref() {
        if !haystack.by_ref().any(|h| h == ch) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn collect_session_jsonl_files_walks_nested_session_dirs() {
        let temp = TempDir::new().expect("tempdir");
        let nested = temp.path().join("--repo--").join("archived");
        std::fs::create_dir_all(&nested).expect("create nested dirs");

        let root_file = temp.path().join("top-level.jsonl");
        let nested_file = nested.join("nested.jsonl");
        let ignored_file = nested.join("notes.txt");

        std::fs::write(&root_file, "{}").expect("write root session");
        std::fs::write(&nested_file, "{}").expect("write nested session");
        std::fs::write(&ignored_file, "ignore").expect("write ignored file");

        let files = collect_session_jsonl_files(temp.path()).expect("collect jsonl files");
        assert_eq!(files, vec![nested_file, root_file]);
    }

    #[test]
    fn config_ui_app_empty_packages_shows_empty_message() {
        let result_slot = Arc::new(StdMutex::new(None));
        let app = ConfigUiApp::new(
            Vec::new(),
            "provider=(default)  model=(default)  thinking=(default)".to_string(),
            result_slot,
        );

        let view = app.view();
        assert!(
            view.contains("Pi Config UI"),
            "missing config ui header:\n{view}"
        );
        assert!(
            view.contains("No package resources discovered. Press Enter to exit."),
            "missing empty packages hint:\n{view}"
        );
    }

    #[test]
    fn config_ui_app_toggle_selected_updates_resource_state() {
        let result_slot = Arc::new(StdMutex::new(None));
        let mut app = ConfigUiApp::new(
            vec![ConfigPackageState {
                scope: SettingsScope::Project,
                source: "local:demo".to_string(),
                resources: vec![
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/a.js".to_string(),
                        enabled: true,
                    },
                    ConfigResourceState {
                        kind: ConfigResourceKind::Skills,
                        path: "skills/demo/SKILL.md".to_string(),
                        enabled: false,
                    },
                ],
            }],
            "provider=(default)  model=(default)  thinking=(default)".to_string(),
            result_slot,
        );

        assert!(
            app.packages[0].resources[0].enabled,
            "first resource should start enabled"
        );
        app.toggle_selected();
        assert!(
            !app.packages[0].resources[0].enabled,
            "toggling selected resource should flip enabled flag"
        );

        app.move_selection(1);
        app.toggle_selected();
        assert!(
            app.packages[0].resources[1].enabled,
            "second resource should toggle on after moving selection"
        );
    }

    #[test]
    fn format_settings_summary_uses_effective_config_values() {
        let config = Config {
            default_provider: Some("openai".to_string()),
            default_model: Some("gpt-4.1".to_string()),
            default_thinking_level: Some("high".to_string()),
            ..Config::default()
        };

        assert_eq!(
            format_settings_summary(&config),
            "provider=openai  model=gpt-4.1  thinking=high"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn persist_package_toggles_writes_filters_per_scope() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path().join("repo");
        let global_dir = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&global_dir).expect("create global dir");
        std::fs::create_dir_all(cwd.join(".pi")).expect("create project .pi");

        std::fs::write(
            global_dir.join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "packages": ["npm:foo"]
            }))
            .expect("serialize global settings"),
        )
        .expect("write global settings");

        std::fs::write(
            cwd.join(".pi").join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "packages": [
                    {
                        "source": "npm:bar",
                        "local": true,
                        "kind": "npm"
                    }
                ]
            }))
            .expect("serialize project settings"),
        )
        .expect("write project settings");

        let packages = vec![
            ConfigPackageState {
                scope: SettingsScope::Global,
                source: "npm:foo".to_string(),
                resources: vec![
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/a.js".to_string(),
                        enabled: true,
                    },
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/b.js".to_string(),
                        enabled: false,
                    },
                ],
            },
            ConfigPackageState {
                scope: SettingsScope::Project,
                source: "npm:bar".to_string(),
                resources: vec![ConfigResourceState {
                    kind: ConfigResourceKind::Skills,
                    path: "skills/demo/SKILL.md".to_string(),
                    enabled: true,
                }],
            },
        ];

        persist_package_toggles_with_roots(&cwd, &global_dir, &packages)
            .expect("persist package toggles");

        let global_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(global_dir.join("settings.json")).expect("read global"),
        )
        .expect("parse global json");
        let global_pkg = global_value["packages"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_object)
            .expect("global package object");
        assert_eq!(
            global_pkg
                .get("source")
                .and_then(serde_json::Value::as_str)
                .expect("source"),
            "npm:foo"
        );
        assert_eq!(
            global_pkg
                .get("extensions")
                .and_then(serde_json::Value::as_array)
                .expect("extensions")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["extensions/a.js"]
        );

        let project_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(cwd.join(".pi").join("settings.json")).expect("read project"),
        )
        .expect("parse project json");
        let project_pkg = project_value["packages"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_object)
            .expect("project package object");
        assert_eq!(
            project_pkg
                .get("source")
                .and_then(serde_json::Value::as_str)
                .expect("source"),
            "npm:bar"
        );
        assert_eq!(
            project_pkg
                .get("skills")
                .and_then(serde_json::Value::as_array)
                .expect("skills")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["skills/demo/SKILL.md"]
        );
        assert!(
            project_pkg
                .get("local")
                .and_then(serde_json::Value::as_bool)
                .expect("local")
        );
    }
}
