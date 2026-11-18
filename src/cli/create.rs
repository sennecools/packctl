//! `packctl create` — interactively create a server profile.
//!
//! The command resolves a pack from a URL, project ID, or slug (the provider
//! is detected from the source when not given), asks for the server root,
//! overlay, and controller details, and writes a profile file. Everything can
//! be supplied via flags for non-interactive use.

use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::cli::prompt::{dialoguer_error, read_secret};
use crate::config::profile::{
    CommandConfig, ControllerKind, DEFAULT_DROP_DIR, ProfileDraft, ProviderKind, SecretsSection,
    write_local_profile, write_profile,
};
use crate::error::{PackError, Result};
use crate::providers::curseforge::client::CfClient;
use crate::providers::curseforge::endpoint::{
    CfEndpoint, ENV_API_BASE, ENV_PROXY_SEND_KEY, resolve_endpoint,
};
use crate::providers::curseforge::models::{CLASS_ID_MODPACKS, CfMod};
use crate::providers::curseforge::source::{ProjectSource, parse_project_source};
use crate::providers::modrinth::client::MrClient;
use crate::providers::modrinth::source as modrinth_source;
use crate::providers::{detect_provider, sanitize};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Arguments for `packctl create`.
pub struct CreateArgs {
    pub name: Option<String>,
    pub non_interactive: bool,
    pub force: bool,
    /// Write the profile into the global profile directory instead of the
    /// current directory.
    pub global: bool,
    pub source: Option<String>,
    /// Pack provider; detected from `source` when omitted, else CurseForge.
    pub provider: Option<ProviderKind>,
    /// Local archive path (.zip or .mrpack file, or a directory of them) for `--provider local`.
    /// Defaults to `<server root>/packs`, where zips are dropped for updates.
    pub archive: Option<PathBuf>,
    /// CurseForge API key to store (encrypted) with the new profile.
    pub apikey: Option<String>,
    pub root: Option<PathBuf>,
    pub overlay: Option<PathBuf>,
    pub controller: Option<ControllerKind>,
    pub instance: Option<String>,
    pub status: Option<String>,
    pub stop: Option<String>,
    pub start: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Run the adoption dry run for this version (or `detect`) after
    /// writing the profile.
    pub adopt: Option<String>,
}

/// A pack resolved from a user-supplied source.
struct ResolvedPack {
    name: String,
    slug: String,
    /// The provider's project id; `None` for local archives.
    project_id: Option<String>,
}

/// Result of choosing a server controller.
struct ControllerSelection {
    kind: ControllerKind,
    instance: Option<String>,
    command: Option<CommandConfig>,
}

/// Command-controller options collected from flags.
struct CommandOptions {
    status: Option<String>,
    stop: Option<String>,
    start: Option<String>,
    timeout_ms: Option<u64>,
}

impl From<&CreateArgs> for CommandOptions {
    fn from(args: &CreateArgs) -> Self {
        Self {
            status: args.status.clone(),
            stop: args.stop.clone(),
            start: args.start.clone(),
            timeout_ms: args.timeout_ms,
        }
    }
}

pub async fn run(args: CreateArgs) -> Result<()> {
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();

    let name = resolve_name(args.name.as_deref(), interactive)?;
    // The source comes first: it decides the provider, and only CurseForge
    // asks for an API key.
    let source = resolve_source(args.provider, args.source.clone(), interactive)?;
    let provider = choose_provider(args.provider, source.as_deref());

    let cwd =
        env::current_dir().map_err(|err| PackError::io("determine current directory", err))?;
    let root = resolve_root(args.root.clone(), &cwd, interactive)?;
    let overlay = resolve_overlay(args.overlay.clone(), &root, interactive)?;

    // How the CurseForge endpoint was chosen, for the summary.
    let mut endpoint_line = None;
    // A version the Modrinth URL pinned.
    let mut pinned_version = None;
    let (pack, archive, api_key) = match provider {
        ProviderKind::CurseForge => {
            let api_key = resolve_api_key(args.apikey.clone(), interactive)?;
            let endpoint = creation_endpoint(api_key.as_deref())?;
            let client = CfClient::new(endpoint.clone());
            endpoint_line = Some(endpoint.describe());
            let input = require_source(source)?;
            let pack = resolve_curseforge_pack(&client, &input).await?;
            let prompt = format!(
                "Found '{}' (project {})",
                pack.name,
                pack.project_id.as_deref().unwrap_or("?")
            );
            if !confirm_pack(&prompt, interactive)? {
                outln!("Aborted.");
                return Ok(());
            }
            (pack, None, api_key)
        }
        ProviderKind::Modrinth => {
            if args.apikey.is_some() {
                errln!("note: Modrinth needs no API key; --apikey is ignored");
            }
            let client = MrClient::from_env()?;
            let input = require_source(source)?;
            let (project, pinned) = modrinth_source::resolve(&client, &input).await?;
            let pack = ResolvedPack {
                name: sanitize(&project.title),
                slug: sanitize(&project.slug),
                project_id: Some(project.id.clone()),
            };
            let prompt = format!(
                "Found '{}' (Modrinth project {})",
                pack.name,
                sanitize(&project.id)
            );
            if !confirm_pack(&prompt, interactive)? {
                outln!("Aborted.");
                return Ok(());
            }
            pinned_version = pinned;
            (pack, None, None)
        }
        ProviderKind::Local => {
            // A source that names an archive (`--source pack.zip`) is the
            // archive unless --archive says otherwise.
            let archive_flag = args
                .archive
                .clone()
                .or_else(|| source.as_ref().map(PathBuf::from));
            let archive = resolve_archive(archive_flag.clone(), &root, interactive)?;
            ensure_archive_ready(&archive)?;
            let pack = ResolvedPack {
                name: if archive_flag.is_some() {
                    archive_display_name(&archive)
                } else {
                    name.clone()
                },
                slug: String::new(),
                project_id: None,
            };
            (pack, Some(archive), None)
        }
    };

    let controller = select_controller(&args, &name, interactive).await?;

    let secrets = match &api_key {
        Some(key) => Some(SecretsSection {
            api_key: Some(crate::config::secrets::encrypt_string(key.trim())?),
        }),
        None => None,
    };

    let draft = ProfileDraft {
        name,
        server_root: if args.global {
            root.clone()
        } else {
            relativize(&cwd, &root)
        },
        provider,
        project_id: pack.project_id.clone(),
        slug: (!pack.slug.is_empty()).then(|| pack.slug.clone()),
        archive: match &archive {
            Some(path) if !args.global => Some(relativize(&cwd, path)),
            Some(path) => Some(path.clone()),
            None => None,
        },
        overlay_path: if args.global {
            overlay.clone()
        } else {
            relativize(&cwd, &overlay)
        },
        controller: controller.kind,
        instance: controller.instance,
        command: controller.command,
        secrets,
        api_base: None,
        optional_files: Default::default(),
    };

    let path = if args.global {
        write_profile(&draft, args.force)?
    } else {
        write_local_profile(&draft, args.force, &cwd)?
    };

    outln!();
    outln!("Created profile '{}'", draft.name);
    outln!("  file:       {}", path.display());
    outln!("  pack:       {}", pack_description(provider, &pack));
    outln!("  server:     {}", root.display());
    outln!("  overlay:    {}", overlay.display());
    if let Some(archive) = &archive {
        outln!("  archive:    {}", archive.display());
    }
    outln!("  controller: {}", controller_description(&draft));
    if provider == ProviderKind::CurseForge {
        if api_key.is_some() {
            outln!("  api key:    stored (encrypted)");
        } else {
            outln!(
                "  api key:    not stored, using {}",
                endpoint_line.as_deref().unwrap_or("the community proxy")
            );
        }
    }
    if provider == ProviderKind::Modrinth {
        outln!("  api key:    not needed (Modrinth)");
    }
    outln!();
    if args.global {
        outln!("Next: packctl status {}", draft.name);
    } else {
        outln!("Next: packctl status");
    }
    if let Some(version) = &pinned_version {
        let profile_arg = args.global.then_some(draft.name.as_str());
        outln!("{}", pinned_version_note(version, profile_arg));
    }
    if provider == ProviderKind::Local
        && let Some(archive) = &archive
    {
        outln!(
            "Server packs (.zip or .mrpack) are read from '{}'.",
            archive.display()
        );
        outln!(
            "If this server already runs the pack, run 'packctl adopt --detect' before the first update."
        );
    }

    // A server that already runs the pack should be adopted before its
    // first update.
    let adopt_target = match args.adopt.clone() {
        Some(target) => Some(target),
        None if interactive && crate::cli::adopt::has_installed_mods(&root) => ask_adopt_version()?,
        None => None,
    };
    if let Some(target) = adopt_target.filter(|target| !target.trim().is_empty()) {
        let profile_arg = args.global.then_some(draft.name.as_str());
        // Asked interactively: offer to apply right away. Given as a flag:
        // show the dry run and the command that applies it.
        let offer_apply = args.adopt.is_none();
        if let Err(error) =
            adopt_after_create(&cwd, profile_arg, &draft.name, &target, offer_apply).await
        {
            errln!("warning: could not prepare the adoption: {error}");
        }
    }
    Ok(())
}

/// Asks which version an existing server runs; `None` when skipped.
fn ask_adopt_version() -> Result<Option<String>> {
    let answer: String = dialoguer::Input::new()
        .with_prompt(
            "This server already has mods installed. Which pack version does it run? \
             (version, 'detect', or empty to skip)",
        )
        .allow_empty(true)
        .interact_text()
        .map_err(|err| dialoguer_error("read the installed version", err))?;
    let answer = answer.trim().to_string();
    Ok((!answer.is_empty()).then_some(answer))
}

/// The adopt switches for `target`: a version, or `detect`.
fn adopt_args_for(target: &str) -> crate::cli::adopt::AdoptArgs {
    let target = target.trim();
    let detect = target.eq_ignore_ascii_case("detect");
    crate::cli::adopt::AdoptArgs {
        version: (!detect).then(|| target.to_string()),
        detect,
        ..Default::default()
    }
}

/// Runs the adoption dry run for the profile just written. With
/// `offer_apply`, asks whether to apply it; otherwise prints the command
/// that does.
async fn adopt_after_create(
    cwd: &Path,
    profile_arg: Option<&str>,
    name: &str,
    target: &str,
    offer_apply: bool,
) -> Result<()> {
    use crate::cli::adopt;

    let profile = match profile_arg {
        Some(name) => crate::config::profile::load_profile(name)?,
        None => crate::config::profile::load_local_profile(cwd)?.ok_or_else(|| {
            PackError::NotFound(format!("the new profile '{name}' could not be read back"))
        })?,
    };
    let updater = crate::core::updater::Updater::from_profile(&profile)?;
    let args = adopt_args_for(target);
    outln!();
    let prepared = adopt::prepare(&updater, &args).await?;
    adopt::print_report(&profile, &prepared, &args);
    if offer_apply {
        let args = adopt::AdoptArgs {
            apply: true,
            ..args
        };
        return adopt::apply(&updater, &prepared, &args);
    }
    let server = profile_arg
        .map(|name| format!(" {}", shell_quote(name)))
        .unwrap_or_default();
    outln!(
        "Next: packctl adopt{server} --as {} --apply",
        shell_quote(&prepared.adoption.report.version.id)
    );
    Ok(())
}

/// The note shown when a Modrinth URL pinned a version: packctl follows the
/// latest version, so the pinned one is installed with `--to`. The version
/// comes from the URL, so it is quoted for a POSIX shell before it is
/// suggested as a command.
fn pinned_version_note(version: &str, profile: Option<&str>) -> String {
    let profile = profile
        .map(|name| format!("{} ", shell_quote(name)))
        .unwrap_or_default();
    format!(
        "Note: packctl follows the latest version; install {} with: packctl update {profile}--to {}",
        sanitize(version),
        shell_quote(version)
    )
}

/// Quotes `value` for a POSIX shell: unchanged when it only holds characters
/// that are never special, else in single quotes with `'` written as `'\''`.
/// Control characters are replaced first, so the suggestion stays one line.
pub(crate) fn shell_quote(value: &str) -> String {
    let value = sanitize(value);
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-/:=@,%".contains(c));
    if plain {
        value
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// The pack source: `--source`, else a prompt when interactive. A local
/// provider needs none (its archive comes from `--archive` or the default
/// drop folder).
fn resolve_source(
    provider: Option<ProviderKind>,
    flag: Option<String>,
    interactive: bool,
) -> Result<Option<String>> {
    if flag.is_some() || provider == Some(ProviderKind::Local) || !interactive {
        return Ok(flag);
    }
    let prompt = match provider {
        Some(ProviderKind::CurseForge) => "CurseForge modpack URL or project ID",
        Some(ProviderKind::Modrinth) => "Modrinth modpack URL, project ID or slug",
        _ => "Modpack (CurseForge or Modrinth URL, or a CurseForge project ID or slug)",
    };
    let input: String = dialoguer::Input::new()
        .with_prompt(prompt)
        .interact_text()
        .map_err(|err| dialoguer_error("read pack source", err))?;
    Ok(Some(input.trim().to_string()))
}

/// The source a remote provider needs.
fn require_source(source: Option<String>) -> Result<String> {
    source
        .filter(|source| !source.trim().is_empty())
        .ok_or_else(|| {
            PackError::Config(
                "pack source is required in non-interactive mode; pass --source <url|id>"
                    .to_string(),
            )
        })
}

/// Asks to confirm the resolved pack when interactive.
fn confirm_pack(prompt: &str, interactive: bool) -> Result<bool> {
    if !interactive {
        return Ok(true);
    }
    dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(true)
        .interact()
        .map_err(|err| dialoguer_error("confirm pack selection", err))
}

/// The provider to use: the `--provider` flag, else the one `--source`
/// names (see [`detect_provider`]), else CurseForge.
fn choose_provider(flag: Option<ProviderKind>, source: Option<&str>) -> ProviderKind {
    flag.or_else(|| source.and_then(detect_provider))
        .unwrap_or(ProviderKind::CurseForge)
}

/// The `pack:` line of the creation summary.
fn pack_description(provider: ProviderKind, pack: &ResolvedPack) -> String {
    let id = pack.project_id.as_deref().unwrap_or("?");
    match provider {
        ProviderKind::CurseForge => format!("{} (CurseForge project {id})", pack.name),
        ProviderKind::Modrinth => format!("{} (Modrinth project {id})", pack.name),
        ProviderKind::Local => format!("{} (local archive)", pack.name),
    }
}

/// Resolves the local archive path, prompting when interactive and omitted.
///
/// With no `--archive`, a local profile reads server-pack zips dropped into
/// `<server root>/packs`; an explicitly supplied path must already exist as a
/// zip file or a directory (so a typo is caught during setup).
fn resolve_archive(flag: Option<PathBuf>, root: &Path, interactive: bool) -> Result<PathBuf> {
    let default = root.join(DEFAULT_DROP_DIR);
    match flag {
        Some(path) => {
            let cwd = env::current_dir()
                .map_err(|err| PackError::io("determine current directory", err))?;
            let resolved = absolute_path(&path, &cwd);
            require_archive_path(&resolved)?;
            Ok(resolved)
        }
        None if interactive => {
            let input: String = dialoguer::Input::new()
                .with_prompt("Server pack archive (.zip or .mrpack file, or a directory of them)")
                .default(default.display().to_string())
                .interact_text()
                .map_err(|err| dialoguer_error("read archive path", err))?;
            Ok(absolute_path(Path::new(&input.trim()), &cwd_env()?))
        }
        None => Ok(default),
    }
}

/// Makes sure the resolved archive path is usable as the drop location.
///
/// An existing zip file or directory is left untouched; a missing path becomes
/// an empty directory so server-pack zips can be dropped into it.
fn ensure_archive_ready(archive: &Path) -> Result<()> {
    match std::fs::metadata(archive) {
        Ok(meta) if meta.is_file() || meta.is_dir() => Ok(()),
        Ok(_) => Err(PackError::Config(format!(
            "archive '{}' is neither a file nor a directory",
            archive.display()
        ))),
        Err(_) => std::fs::create_dir_all(archive).map_err(|err| {
            PackError::io(format!("create drop folder '{}'", archive.display()), err)
        }),
    }
}

/// Validates that an explicitly supplied archive path exists as a file or a
/// directory.
fn require_archive_path(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|err| {
        PackError::Config(format!(
            "archive '{}' is not accessible: {err}",
            path.display()
        ))
    })?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(PackError::Config(format!(
            "archive '{}' is neither a file nor a directory",
            path.display()
        )));
    }
    Ok(())
}

fn cwd_env() -> Result<PathBuf> {
    env::current_dir().map_err(|err| PackError::io("determine current directory", err))
}

/// A human-friendly label for a local archive path: the file stem when it looks
/// like a file (has an extension), otherwise the directory name.
fn archive_display_name(path: &Path) -> String {
    if path.extension().is_some() {
        path.file_stem()
            .or_else(|| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| path.display().to_string())
    } else {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| path.display().to_string())
    }
}

/// Resolves the optional API key to store with the profile.
fn resolve_api_key(flag: Option<String>, interactive: bool) -> Result<Option<String>> {
    if let Some(key) = flag {
        let key = key.trim();
        if key.is_empty() {
            return Err(PackError::Config("API key must not be empty".to_string()));
        }
        return Ok(Some(key.to_string()));
    }
    if interactive {
        let input = read_secret(
            "CurseForge API key (optional; leave empty to use the community proxy)",
            true,
        )?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    } else {
        Ok(None)
    }
}

/// The CurseForge endpoint used while creating the profile: the key from
/// `--apikey`, else `$CF_API_KEY`, else the shared global key decides
/// between the official API and the community proxy, unless
/// `$PACKCTL_CF_API_BASE` names another endpoint.
fn creation_endpoint(flag_key: Option<&str>) -> Result<CfEndpoint> {
    let key = match flag_key {
        Some(key) => Some(key.to_string()),
        None => env::var("CF_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .or_else(|| match crate::config::secrets::load_global_key() {
                Ok(key) => key,
                Err(err) => {
                    errln!(
                        "note: the shared API key could not be read ({err}); continuing without it"
                    );
                    None
                }
            }),
    }
    .map(|key| key.trim().to_string());
    let env_base = env::var(ENV_API_BASE).ok();
    let send_key = env::var(ENV_PROXY_SEND_KEY).is_ok_and(|value| value.trim() == "1");
    resolve_endpoint(env_base.as_deref(), None, key, send_key)
}

/// Rejects projects that are not modpacks, and warns when the author has
/// disabled downloads through third-party tools.
fn check_modpack(project: &CfMod) -> Result<()> {
    if let Some(class) = project.class_id
        && class != CLASS_ID_MODPACKS
    {
        return Err(PackError::Config(format!(
            "CurseForge project {} is not a modpack (class {class})",
            project.id
        )));
    }
    if project.allow_mod_distribution == Some(false) {
        errln!(
            "warning: the author of '{}' disabled downloads through third-party tools, so \
             packctl cannot download its server packs from CurseForge; download them yourself \
             and use the local provider (drop them in packs/)",
            sanitize(&project.name)
        );
    }
    Ok(())
}

/// Resolves the profile name, prompting when interactive and omitted.
fn resolve_name(name: Option<&str>, interactive: bool) -> Result<String> {
    match name {
        Some(name) if !name.trim().is_empty() => Ok(name.to_string()),
        Some(_) => Err(PackError::Config(
            "profile name must not be empty".to_string(),
        )),
        None if interactive => dialoguer::Input::new()
            .with_prompt("Server profile name")
            .default(default_profile_name())
            .interact_text()
            .map_err(|err| dialoguer_error("read profile name", err)),
        None => Ok(default_profile_name()),
    }
}

/// The current directory name, used as the default profile name.
fn default_profile_name() -> String {
    env::current_dir()
        .ok()
        .and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "server".to_string())
}

/// Resolves a CurseForge source (URL, project id or slug) to a project.
async fn resolve_curseforge_pack(client: &CfClient, input: &str) -> Result<ResolvedPack> {
    match parse_project_source(input)? {
        ProjectSource::Id(id) => {
            // The id is authoritative; enrichment is best-effort so a numeric
            // id works even when the endpoint is unreachable.
            match client.get_mod(id).await {
                Ok(mod_info) => {
                    check_modpack(&mod_info)?;
                    Ok(ResolvedPack {
                        name: sanitize(&mod_info.name),
                        slug: sanitize(&mod_info.slug),
                        project_id: Some(id.to_string()),
                    })
                }
                Err(err) => {
                    errln!(
                        "note: could not look up project {id} ({err}); creating the profile anyway"
                    );
                    Ok(ResolvedPack {
                        name: format!("project {id}"),
                        slug: String::new(),
                        project_id: Some(id.to_string()),
                    })
                }
            }
        }
        ProjectSource::Slug(slug) => {
            let mod_info = client.search_by_slug(&slug).await.map_err(|err| {
                PackError::Provider(format!(
                    "could not resolve CurseForge slug '{slug}' via {}: {err}. Pass the numeric \
                     project id with --source <id>, or set PACKCTL_CF_API_BASE / store a key \
                     with 'packctl apikey --global'",
                    client.endpoint().base_url
                ))
            })?;
            check_modpack(&mod_info)?;
            Ok(ResolvedPack {
                name: sanitize(&mod_info.name),
                slug: sanitize(&mod_info.slug),
                project_id: Some(mod_info.id.to_string()),
            })
        }
    }
}

/// Resolves the server root, defaulting to the current directory.
fn resolve_root(root: Option<PathBuf>, cwd: &Path, interactive: bool) -> Result<PathBuf> {
    let resolved = match root {
        Some(root) => absolute_path(&root, cwd),
        None if interactive => {
            let input: String = dialoguer::Input::new()
                .with_prompt("Server root")
                .default(cwd.display().to_string())
                .interact_text()
                .map_err(|err| dialoguer_error("read server root", err))?;
            absolute_path(Path::new(&input), cwd)
        }
        None => cwd.to_path_buf(),
    };
    Ok(resolved)
}

/// Resolves the overlay directory, defaulting to `<root>/overlay`.
///
/// A relative path, from `--overlay` or typed at the prompt, is resolved
/// against the server root, so `--overlay overlay` means `<root>/overlay`
/// wherever `create` runs.
fn resolve_overlay(overlay: Option<PathBuf>, root: &Path, interactive: bool) -> Result<PathBuf> {
    let default = root.join("overlay");
    let resolved = match overlay {
        Some(overlay) => absolute_path(&overlay, root),
        None if interactive => {
            let input: String = dialoguer::Input::new()
                .with_prompt("Overlay directory (relative to the server root)")
                .default(default.display().to_string())
                .interact_text()
                .map_err(|err| dialoguer_error("read overlay directory", err))?;
            absolute_path(Path::new(input.trim()), root)
        }
        None => default,
    };
    Ok(resolved)
}

/// Makes `path` absolute against `cwd` when it is relative.
fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Rewrites `path` relative to `base` when it is inside it, so a config file
/// stored next to the server root can use natural values like `"."` and
/// `"overlay"`.
fn relativize(base: &Path, path: &Path) -> PathBuf {
    match path.strip_prefix(base) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => path.to_path_buf(),
    }
}

/// Chooses the controller, prompting when interactive.
async fn select_controller(
    args: &CreateArgs,
    name: &str,
    interactive: bool,
) -> Result<ControllerSelection> {
    let kind = match &args.controller {
        Some(kind) => *kind,
        None if interactive => {
            let items = ["amp", "command"];
            let chosen = dialoguer::Select::new()
                .with_prompt("Server controller")
                .items(&items)
                .default(0)
                .interact()
                .map_err(|err| dialoguer_error("select controller", err))?;
            match items[chosen] {
                "amp" => ControllerKind::Amp,
                _ => ControllerKind::Command,
            }
        }
        None => {
            return Err(PackError::Config(
                "controller is required in non-interactive mode; pass --controller amp|command"
                    .to_string(),
            ));
        }
    };

    match kind {
        ControllerKind::Amp => {
            let instance = match &args.instance {
                Some(instance) => instance.clone(),
                None if interactive => dialoguer::Input::new()
                    .with_prompt("AMP instance name")
                    .default(name.to_string())
                    .interact_text()
                    .map_err(|err| dialoguer_error("read AMP instance", err))?,
                None => {
                    return Err(PackError::Config(
                        "AMP controller requires --instance in non-interactive mode".to_string(),
                    ));
                }
            };
            Ok(ControllerSelection {
                kind,
                instance: Some(instance),
                command: None,
            })
        }
        ControllerKind::Command => {
            let options = CommandOptions::from(args);
            let status = get_command(
                options.status,
                "--status",
                "Status command",
                "pgrep -f server.jar",
                interactive,
            )?;
            let stop = get_command(
                options.stop,
                "--stop",
                "Stop command",
                &format!("screen -S {name} -X stuff \"stop\\n\""),
                interactive,
            )?;
            let start = get_command(
                options.start,
                "--start",
                "Start command",
                &format!("screen -S {name} -X stuff \"start\\n\""),
                interactive,
            )?;
            let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
            Ok(ControllerSelection {
                kind,
                instance: None,
                command: Some(CommandConfig {
                    status,
                    stop,
                    start,
                    timeout_ms: Some(timeout_ms),
                }),
            })
        }
    }
}

/// Resolves a command-controller argv from a flag or an interactive prompt.
fn get_command(
    flag: Option<String>,
    flag_name: &str,
    prompt: &str,
    default: &str,
    interactive: bool,
) -> Result<Vec<String>> {
    let line = match flag {
        Some(value) => value,
        None if interactive => dialoguer::Input::new()
            .with_prompt(prompt)
            .default(default.to_string())
            .interact_text()
            .map_err(|err| dialoguer_error("read command", err))?,
        None => {
            return Err(PackError::Config(format!(
                "command controller requires {flag_name} in non-interactive mode"
            )));
        }
    };
    let argv = argv_split(&line);
    if argv.is_empty() {
        return Err(PackError::Config(format!("{prompt} must not be empty")));
    }
    Ok(argv)
}

/// Splits a shell-like command line into argv tokens.
///
/// Whitespace separates tokens; double quotes group tokens and support the
/// `\n`, `\t`, `\\`, and `\"` escapes. Commands are never run through a shell,
/// so this splitter is intentionally small and predictable.
fn argv_split(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;

    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '\\' => match chars.peek() {
                Some('n') => {
                    chars.next();
                    current.push('\n');
                }
                Some('t') => {
                    chars.next();
                    current.push('\t');
                }
                Some('"') => {
                    chars.next();
                    current.push('"');
                }
                Some('\\') => {
                    chars.next();
                    current.push('\\');
                }
                _ => current.push('\\'),
            },
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn controller_description(draft: &ProfileDraft) -> String {
    match &draft.command {
        Some(_) => "command".to_string(),
        None => format!("amp (instance {})", draft.instance.as_deref().unwrap_or("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apikey_flag_is_trimmed_and_must_not_be_blank() {
        assert_eq!(
            resolve_api_key(Some(" KEY\n".to_string()), false).unwrap(),
            Some("KEY".to_string())
        );
        assert!(resolve_api_key(Some(" \t\n".to_string()), false).is_err());
        assert_eq!(resolve_api_key(None, false).unwrap(), None);
    }

    #[test]
    fn argv_split_handles_quotes_and_escapes() {
        assert_eq!(
            argv_split("screen -S atm10 -X stuff \"stop\\n\""),
            vec!["screen", "-S", "atm10", "-X", "stuff", "stop\n",]
        );
        assert_eq!(
            argv_split("pgrep -f 'server.jar'"),
            vec!["pgrep", "-f", "'server.jar'"]
        );
        assert_eq!(argv_split("echo \"a b\"  c"), vec!["echo", "a b", "c"]);
        assert_eq!(argv_split("  "), Vec::<String>::new());
        assert_eq!(argv_split(""), Vec::<String>::new());
    }

    #[test]
    fn provider_comes_from_flag_then_source_then_curseforge() {
        let modrinth = Some("https://modrinth.com/modpack/fabulously-optimized");
        assert_eq!(choose_provider(None, None), ProviderKind::CurseForge);
        assert_eq!(
            choose_provider(None, Some("925200")),
            ProviderKind::CurseForge
        );
        assert_eq!(choose_provider(None, modrinth), ProviderKind::Modrinth);
        assert_eq!(choose_provider(None, Some("pack.zip")), ProviderKind::Local);
        // The flag always wins.
        assert_eq!(
            choose_provider(Some(ProviderKind::CurseForge), modrinth),
            ProviderKind::CurseForge
        );
    }

    #[test]
    fn pack_description_names_the_provider() {
        let pack = |id: Option<&str>| ResolvedPack {
            name: "ATM10".to_string(),
            slug: String::new(),
            project_id: id.map(str::to_string),
        };
        assert_eq!(
            pack_description(ProviderKind::CurseForge, &pack(Some("925200"))),
            "ATM10 (CurseForge project 925200)"
        );
        assert_eq!(
            pack_description(ProviderKind::Modrinth, &pack(Some("1KVo5zza"))),
            "ATM10 (Modrinth project 1KVo5zza)"
        );
        assert_eq!(
            pack_description(ProviderKind::Local, &pack(None)),
            "ATM10 (local archive)"
        );
    }

    #[test]
    fn archive_display_name_uses_stem_for_files_and_dir_name_for_directories() {
        assert_eq!(
            archive_display_name(Path::new("/packs/FTB StoneBlock 4 1.19.1.zip")),
            "FTB StoneBlock 4 1.19.1"
        );
        assert_eq!(
            archive_display_name(Path::new("/packs/sb4-archives")),
            "sb4-archives"
        );
        assert_eq!(
            archive_display_name(Path::new("/packs/archive.zip")),
            "archive"
        );
    }

    #[test]
    fn resolve_archive_requires_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("pack.zip");
        std::fs::write(&existing, b"zip").unwrap();

        let root = dir.path().to_path_buf();
        let resolved = resolve_archive(Some(existing.clone()), &root, false).unwrap();
        assert_eq!(resolved, existing);

        let missing = resolve_archive(Some(dir.path().join("nope.zip")), &root, false);
        assert!(matches!(missing, Err(PackError::Config(_))));
    }

    #[test]
    fn resolve_archive_without_flag_defaults_to_packs_drop_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("server");

        let resolved = resolve_archive(None, &root, false).unwrap();
        assert_eq!(resolved, root.join(DEFAULT_DROP_DIR));
    }

    #[test]
    fn ensure_archive_ready_creates_missing_drop_folder() {
        let dir = tempfile::tempdir().unwrap();
        let drop = dir.path().join("server").join("packs");
        assert!(!drop.exists());

        ensure_archive_ready(&drop).unwrap();
        assert!(drop.is_dir());

        // Already existing paths (file or directory) are left untouched.
        let file = dir.path().join("pack.zip");
        std::fs::write(&file, b"zip").unwrap();
        ensure_archive_ready(&file).unwrap();
        assert!(file.is_file());
    }

    #[test]
    fn absolute_path_joins_against_cwd() {
        assert_eq!(
            absolute_path(Path::new("server"), Path::new("/srv")),
            PathBuf::from("/srv/server")
        );
        assert_eq!(
            absolute_path(Path::new("/abs/path"), Path::new("/srv")),
            PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn controller_description_reports_kind() {
        let amp = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv"),
            provider: ProviderKind::CurseForge,
            project_id: Some("1".to_string()),
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("ATM10".to_string()),
            command: None,
            secrets: None,
            api_base: None,
            optional_files: Default::default(),
        };
        assert_eq!(controller_description(&amp), "amp (instance ATM10)");

        let command = ProfileDraft {
            controller: ControllerKind::Command,
            instance: None,
            command: Some(CommandConfig {
                status: vec!["pgrep".to_string()],
                stop: vec!["stop".to_string()],
                start: vec!["start".to_string()],
                timeout_ms: None,
            }),
            ..amp
        };
        assert_eq!(controller_description(&command), "command");
    }

    #[test]
    fn shell_quote_leaves_plain_values_and_quotes_the_rest() {
        assert_eq!(shell_quote("1.6.1+1.21.1"), "1.6.1+1.21.1");
        assert_eq!(shell_quote("YQpPe5Iu"), "YQpPe5Iu");
        assert_eq!(shell_quote("Pack 1.0"), "'Pack 1.0'");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a\nb"), "'a b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn pinned_version_note_suggests_a_quoted_command() {
        assert_eq!(
            pinned_version_note("1.6.1", None),
            "Note: packctl follows the latest version; install 1.6.1 with: packctl update --to 1.6.1"
        );
        assert_eq!(
            pinned_version_note("x;y", Some("my server")),
            "Note: packctl follows the latest version; install x;y with: packctl update 'my server' --to 'x;y'"
        );
    }

    #[test]
    fn source_is_not_prompted_for_without_a_terminal_or_for_local() {
        assert_eq!(resolve_source(None, None, false).unwrap(), None);
        assert_eq!(
            resolve_source(Some(ProviderKind::Local), None, true).unwrap(),
            None
        );
        assert_eq!(
            resolve_source(None, Some("925200".to_string()), true).unwrap(),
            Some("925200".to_string())
        );
        assert!(matches!(
            require_source(Some("  ".to_string())),
            Err(PackError::Config(_))
        ));
    }

    fn modrinth_args(source: &str, root: &Path) -> CreateArgs {
        CreateArgs {
            name: Some("cobble".to_string()),
            non_interactive: true,
            force: false,
            global: true,
            source: Some(source.to_string()),
            provider: None,
            archive: None,
            apikey: None,
            root: Some(root.to_path_buf()),
            overlay: None,
            controller: Some(ControllerKind::Command),
            instance: None,
            status: Some("true".to_string()),
            stop: Some("true".to_string()),
            start: Some("true".to_string()),
            timeout_ms: None,
            adopt: None,
        }
    }

    #[test]
    fn modrinth_source_detected_from_url() {
        use std::ffi::OsStr;

        use crate::providers::modrinth::client::ENV_API_BASE;
        use crate::providers::modrinth::models::fixtures;
        use crate::testutil::http_server::{Route, serve};
        use crate::testutil::{EnvGuard, env_lock};

        let url = "https://modrinth.com/modpack/cobblemon-modpack/version/1.6.1";
        assert_eq!(choose_provider(None, Some(url)), ProviderKind::Modrinth);

        let _lock = env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("PACKCTL_HOME", home.path().as_os_str());
        // A CurseForge key in the environment must not matter for Modrinth.
        let _key = EnvGuard::set("CF_API_KEY", OsStr::new("not-used"));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let base = runtime.block_on(serve(vec![Route::json(
            "/v2/project/cobblemon-modpack",
            fixtures::PROJECT,
        )]));
        let _api = EnvGuard::set(ENV_API_BASE, OsStr::new(&format!("{base}/v2")));

        let root = home.path().join("server");
        runtime
            .block_on(run(modrinth_args(url, &root)))
            .expect("create succeeds without a key");

        let written = std::fs::read_to_string(home.path().join("cobble.toml")).unwrap();
        assert!(written.contains("provider = \"modrinth\""), "{written}");
        assert!(written.contains("project_id = \"5FFgwNNP\""), "{written}");
        assert!(
            written.contains("slug = \"cobblemon-modpack\""),
            "{written}"
        );
        assert!(!written.contains("secrets"), "{written}");
        assert!(!written.contains("optional_files"), "{written}");

        // A client-only pack is refused before anything is written.
        let client_only = fixtures::PROJECT
            .replace(
                "\"server_side\": \"required\"",
                "\"server_side\": \"unsupported\"",
            )
            .replace("cobblemon-modpack", "client-pack");
        let base = runtime.block_on(serve(vec![Route::json(
            "/v2/project/client-pack",
            &client_only,
        )]));
        let _api = EnvGuard::set(ENV_API_BASE, OsStr::new(&format!("{base}/v2")));
        let mut args = modrinth_args("https://modrinth.com/modpack/client-pack", &root);
        args.name = Some("client".to_string());
        let err = runtime.block_on(run(args)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "'client-pack' is a client-only Modrinth modpack (server_side = unsupported)"
        );
        assert!(!home.path().join("client.toml").exists());
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    #[test]
    fn relative_overlay_flag_resolves_against_root() {
        let root = std::env::temp_dir().join("packctl-create-root");
        assert_eq!(
            resolve_overlay(Some(PathBuf::from("overlay")), &root, false).unwrap(),
            root.join("overlay")
        );
        assert_eq!(
            resolve_overlay(Some(PathBuf::from("custom/ov")), &root, false).unwrap(),
            root.join("custom").join("ov")
        );
        // An absolute overlay path is kept as given.
        let elsewhere = std::env::temp_dir().join("packctl-overlay-elsewhere");
        assert_eq!(
            resolve_overlay(Some(elsewhere.clone()), &root, false).unwrap(),
            elsewhere
        );
        // Without the flag the default stays <root>/overlay.
        assert_eq!(
            resolve_overlay(None, &root, false).unwrap(),
            root.join("overlay")
        );
    }
}
