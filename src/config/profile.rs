//! Server profile configuration loading.
//!
//! A profile is one TOML file per server, named `<name>.toml`, stored in the
//! packctl profile directory. See design notes "Configuration Model" and
//! "Terminology" for the domain model.

use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{PackError, Result};
use crate::providers::curseforge::endpoint::{CfEndpoint, normalize_base};
pub use crate::providers::modrinth::mrpack::OptionalFiles;

/// Pack provider for a profile's upstream pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// A CurseForge modpack
    #[value(name = "curseforge")]
    CurseForge,
    /// A Modrinth modpack
    Modrinth,
    /// Server-pack archives on this machine
    Local,
}

impl ProviderKind {
    /// The name used in profiles and on the command line.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::CurseForge => "curseforge",
            ProviderKind::Modrinth => "modrinth",
            ProviderKind::Local => "local",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kind of server controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ControllerKind {
    /// An AMP instance, driven through ampinstmgr
    Amp,
    /// Custom status, stop and start commands
    Command,
}

/// Command controller configuration, used when the controller kind is `command`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandConfig {
    pub status: Vec<String>,
    pub stop: Vec<String>,
    pub start: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Where the live server lives.
#[derive(Debug, Clone)]
pub struct ServerSection {
    pub root: PathBuf,
}

/// Default subdirectory inside the server root where server-pack archives are
/// dropped for `local` provider updates.
pub const DEFAULT_DROP_DIR: &str = "packs";

/// Which upstream pack the server follows.
#[derive(Debug, Clone)]
pub struct PackSection {
    pub provider: ProviderKind,
    /// The provider's project id: a number for CurseForge, a base62 id for
    /// Modrinth, and absent for local archives.
    pub project_id: Option<String>,
    pub slug: Option<String>,
    /// Local archive path (.zip or .mrpack file, or a directory of them) for the `local`
    /// provider. When omitted, `local` packs update from `<server root>/packs`.
    pub archive: Option<PathBuf>,
    /// CurseForge API base URL (normalized), overriding the default choice
    /// between the official API and the community proxy.
    pub api_base: Option<String>,
    /// Whether Modrinth files marked optional on the server are installed.
    pub optional_files: OptionalFiles,
}

#[cfg(test)]
impl PackSection {
    /// A CurseForge pack section for tests.
    pub fn curseforge(project_id: u32) -> Self {
        PackSection {
            provider: ProviderKind::CurseForge,
            project_id: Some(project_id.to_string()),
            slug: None,
            archive: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        }
    }

    /// A local-archive pack section for tests.
    pub fn local(archive: Option<PathBuf>) -> Self {
        PackSection {
            provider: ProviderKind::Local,
            project_id: None,
            slug: None,
            archive,
            api_base: None,
            optional_files: OptionalFiles::default(),
        }
    }
}

/// Where the mirrored local overlay lives.
#[derive(Debug, Clone)]
pub struct OverlaySection {
    pub path: PathBuf,
}

/// How the server process is controlled.
#[derive(Debug, Clone)]
pub struct ControllerSection {
    pub kind: ControllerKind,
    pub instance: Option<String>,
    pub command: Option<CommandConfig>,
}

/// Encrypted secrets stored with a profile, such as the CurseForge API key.
///
/// Values are encrypted blobs produced by `config::secrets`; the master key is
/// kept outside the profile file so the API key never sits in plaintext.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsSection {
    /// Encrypted CurseForge API key blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// A fully resolved server profile.
#[derive(Debug, Clone)]
pub struct ServerProfile {
    pub name: String,
    pub server: ServerSection,
    pub pack: PackSection,
    pub overlay: OverlaySection,
    pub controller: ControllerSection,
    pub secrets: SecretsSection,
}

impl ServerProfile {
    /// The CurseForge API key configured for this profile, if any.
    ///
    /// A key is optional: without one, packctl uses the community proxy
    /// (see [`ServerProfile::curseforge_endpoint`]). `$CF_API_KEY` wins when
    /// set; otherwise the profile's stored, encrypted key is decrypted;
    /// otherwise the shared, machine-wide key stored with `packctl apikey
    /// --global` is used. Returns `Ok(None)` when no key is available, and
    /// an error when a stored key cannot be decrypted.
    pub fn curseforge_api_key(&self) -> Result<Option<String>> {
        self.curseforge_api_key_with_source().map(|(key, _)| key)
    }

    /// The CurseForge API key and where it came from, in the order described
    /// at [`ServerProfile::curseforge_api_key`]. Returns
    /// `(None, KeySource::None)` when no key is configured. The key is
    /// trimmed whatever its source, so one stored or exported with stray
    /// whitespace still forms a valid `x-api-key` header.
    pub fn curseforge_api_key_with_source(&self) -> Result<(Option<String>, KeySource)> {
        let trimmed = |key: String| key.trim().to_string();
        let from_env = std::env::var("CF_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty());
        if let Some(key) = from_env {
            return Ok((Some(trimmed(key)), KeySource::Env));
        }
        if let Some(blob) = &self.secrets.api_key {
            let key = crate::config::secrets::decrypt_string(blob)?;
            return Ok((Some(trimmed(key)), KeySource::Profile));
        }
        Ok(match crate::config::secrets::load_global_key()? {
            Some(key) => (Some(trimmed(key)), KeySource::Global),
            None => (None, KeySource::None),
        })
    }

    /// The CurseForge API base configured for this profile:
    /// `$PACKCTL_CF_API_BASE` when set, else the profile's `[pack] api_base`.
    /// `None` means the default choice (official API with a key, community
    /// proxy without).
    pub fn curseforge_api_base(&self) -> Option<String> {
        env_api_base().or_else(|| self.pack.api_base.clone())
    }

    /// The CurseForge endpoint this profile talks to, with the key attached
    /// only where it may be sent (see [`resolve_endpoint`]).
    ///
    /// [`resolve_endpoint`]: crate::providers::curseforge::endpoint::resolve_endpoint
    pub fn curseforge_endpoint(&self) -> Result<CfEndpoint> {
        use crate::providers::curseforge::endpoint::{ENV_PROXY_SEND_KEY, resolve_endpoint};
        let send_key = env::var(ENV_PROXY_SEND_KEY).is_ok_and(|value| value.trim() == "1");
        resolve_endpoint(
            env_api_base().as_deref(),
            self.pack.api_base.as_deref(),
            self.curseforge_api_key()?,
            send_key,
        )
    }
}

/// Where a profile's CurseForge API key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// The `$CF_API_KEY` environment variable.
    Env,
    /// The encrypted key stored in the profile file.
    Profile,
    /// The shared, machine-wide key (`packctl apikey --global`).
    Global,
    /// No key is configured.
    None,
}

impl KeySource {
    /// A short description for `packctl apikey --show`.
    pub fn describe(&self) -> &'static str {
        match self {
            KeySource::Env => "from CF_API_KEY environment variable",
            KeySource::Profile => "stored in profile (encrypted)",
            KeySource::Global => "shared global key",
            KeySource::None => "none",
        }
    }
}

/// `$PACKCTL_CF_API_BASE`, when set to a non-blank value.
fn env_api_base() -> Option<String> {
    env::var(crate::providers::curseforge::endpoint::ENV_API_BASE)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Raw TOML shape of a profile file, before validation and path resolution.
#[derive(Debug, Deserialize)]
struct RawProfile {
    name: Option<String>,
    server: RawServer,
    pack: RawPack,
    overlay: RawOverlay,
    controller: RawController,
    #[serde(default)]
    secrets: SecretsSection,
}

#[derive(Debug, Deserialize)]
struct RawServer {
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RawPack {
    provider: ProviderKind,
    #[serde(default, deserialize_with = "de_project_id")]
    project_id: Option<String>,
    slug: Option<String>,
    #[serde(default)]
    archive: Option<PathBuf>,
    /// CurseForge API base override; validated and normalized at load time.
    #[serde(default)]
    api_base: Option<String>,
    /// Modrinth optional server files: `include` (default) or `exclude`.
    #[serde(default)]
    optional_files: OptionalFiles,
}

/// A project id as written in a profile: a TOML integer (CurseForge) or a
/// string (any provider).
#[derive(Deserialize)]
#[serde(untagged)]
enum IdRepr {
    Int(u64),
    Str(String),
}

/// Reads `project_id` as text. `0` and blank strings mean "not set", so
/// older profiles that wrote `project_id = 0` keep loading.
fn de_project_id<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<IdRepr>::deserialize(deserializer)? {
        None | Some(IdRepr::Int(0)) => None,
        Some(IdRepr::Int(n)) => Some(n.to_string()),
        Some(IdRepr::Str(s)) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
    })
}

#[derive(Debug, Deserialize)]
struct RawOverlay {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RawController {
    #[serde(rename = "type")]
    kind: ControllerKind,
    instance: Option<String>,
    command: Option<CommandConfig>,
}

/// Resolve the directory that contains `<name>.toml` profile files.
///
/// Priority: `$PACKCTL_HOME` if set; else `$XDG_CONFIG_HOME/packctl` (default
/// `$HOME/.config/packctl`) when a home directory is available; else
/// `./packctl`.
pub fn profile_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("PACKCTL_HOME") {
        return Ok(PathBuf::from(dir));
    }

    let xdg_config = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = env::var_os("HOME").map(PathBuf::from);

    Ok(xdg_config
        .map(|base| base.join("packctl"))
        .or_else(|| home.map(|home| home.join(".config").join("packctl")))
        .unwrap_or_else(|| PathBuf::from("./packctl")))
}

/// Load and fully resolve the profile with the given name.
pub fn load_profile(name: &str) -> Result<ServerProfile> {
    validate_profile_name(name)?;
    let dir = profile_dir()?;
    let path = dir.join(format!("{name}.toml"));

    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(PackError::NotFound(format!(
                "profile '{name}' not found in {}",
                dir.display()
            )));
        }
        Err(err) => return Err(PackError::io(format!("read profile '{name}'"), err)),
    };

    let raw: RawProfile =
        toml::from_str(&content).map_err(|err| PackError::Parse(err.to_string()))?;

    raw.into_profile(name, &dir)
}

/// Load every profile in the profile directory, sorted by name.
///
/// Returns an empty list when the profile directory does not exist.
pub fn list_profiles() -> Result<Vec<ServerProfile>> {
    let dir = profile_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(&dir)
        .map_err(|err| PackError::io(format!("list profiles in {}", dir.display()), err))?;

    let mut profiles = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| PackError::io("read profile directory entry", err))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        profiles.push(load_profile(stem)?);
    }

    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

/// Prints the configured server profiles for `packctl list`.
///
/// Global profiles are shown as a table; a `.packctl.toml` in the current
/// directory is mentioned below it.
pub fn list_cli() -> Result<()> {
    let profiles = list_profiles()?;
    if profiles.is_empty() {
        outln!("No server profiles configured.");
        outln!("Create a profile in {}", profile_dir()?.display());
    } else {
        let rows: Vec<[String; 4]> = profiles
            .iter()
            .map(|profile| {
                [
                    profile.name.clone(),
                    profile.pack.provider.to_string(),
                    profile_project(profile),
                    profile.server.root.display().to_string(),
                ]
            })
            .collect();
        out!("{}", profile_table(&rows));
    }
    if let Ok(cwd) = env::current_dir()
        && let Some(line) = local_profile_line(&cwd)
    {
        outln!();
        outln!("{line}");
    }
    Ok(())
}

/// The project a profile follows: its id, else its slug, else `-`.
fn profile_project(profile: &ServerProfile) -> String {
    profile
        .pack
        .project_id
        .clone()
        .or_else(|| profile.pack.slug.clone())
        .unwrap_or_else(|| "-".to_string())
}

/// Renders the `packctl list` table: a header and one row per profile, with
/// every column but the last padded to its widest cell.
fn profile_table(rows: &[[String; 4]]) -> String {
    const HEADER: [&str; 4] = ["NAME", "PROVIDER", "PROJECT", "SERVER ROOT"];
    let mut widths = HEADER.map(|title| title.chars().count());
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let render = |cells: [&str; 4]| {
        let mut line = String::new();
        for (index, cell) in cells.iter().enumerate() {
            if index + 1 == cells.len() {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                let pad = widths[index] - cell.chars().count() + 2;
                line.push_str(&" ".repeat(pad));
            }
        }
        line.trim_end().to_string() + "\n"
    };
    let mut out = render(HEADER);
    for row in rows {
        out.push_str(&render([&row[0], &row[1], &row[2], &row[3]]));
    }
    out
}

/// The `Local profile here: …` line for `packctl list`, when `dir` holds a
/// `.packctl.toml`.
fn local_profile_line(dir: &Path) -> Option<String> {
    match load_local_profile(dir) {
        Ok(Some(profile)) => Some(format!(
            "Local profile here: {} ({} {})",
            profile.name,
            profile.pack.provider,
            profile_project(&profile)
        )),
        Ok(None) => None,
        Err(err) => Some(format!(
            "Local profile here: {LOCAL_PROFILE} could not be loaded: {err}"
        )),
    }
}

/// File name of a profile stored inside the server root.
pub const LOCAL_PROFILE: &str = ".packctl.toml";

/// Loads a profile for a command.
///
/// A named server loads the global profile `<name>.toml` from the profile
/// directory; otherwise the local `.packctl.toml` in the current directory is
/// used. This lets an administrator `cd` into a server root and run commands
/// without registering anything globally.
pub fn resolve_profile(server: Option<&str>) -> Result<ServerProfile> {
    if let Some(name) = server {
        return load_profile(name).map_err(|err| match env::current_dir() {
            Ok(cwd) => with_local_profile_hint(err, name, &cwd),
            Err(_) => err,
        });
    }
    let cwd =
        env::current_dir().map_err(|err| PackError::io("determine current directory", err))?;
    resolve_local_profile_in(&cwd)
}

/// Adds a hint to a missing named profile when `cwd` holds a local profile:
/// the user most likely meant `name` as a version (`packctl plan 1.2.0`).
fn with_local_profile_hint(err: PackError, name: &str, cwd: &Path) -> PackError {
    match err {
        PackError::NotFound(message) if cwd.join(LOCAL_PROFILE).is_file() => {
            PackError::NotFound(format!(
                "{message}\nhint: '{name}' was read as a profile name; to target a version of \
                 the local profile use --to {name}"
            ))
        }
        other => other,
    }
}

fn resolve_local_profile_in(cwd: &Path) -> Result<ServerProfile> {
    load_local_profile(cwd)?.ok_or_else(|| {
        PackError::NotFound(format!(
            "no server name given and no {LOCAL_PROFILE} found in {}; run 'packctl create' \
             to set up a server",
            cwd.display()
        ))
    })
}

/// Loads the local profile file from `dir` (usually the current directory).
pub fn load_local_profile(dir: &Path) -> Result<Option<ServerProfile>> {
    let path = dir.join(LOCAL_PROFILE);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)
        .map_err(|err| PackError::io(format!("read '{}'", path.display()), err))?;
    let raw: RawProfile =
        toml::from_str(&content).map_err(|err| PackError::Parse(err.to_string()))?;
    let fallback_name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "server".to_string());
    raw.into_profile(&fallback_name, dir).map(Some)
}

/// Where the profile file for `server` lives.
///
/// A named server resolves to the global profile directory; otherwise the
/// local file in the current directory.
pub fn profile_file_path(server: Option<&str>) -> Result<PathBuf> {
    match server {
        Some(name) => {
            validate_profile_name(name)?;
            Ok(profile_dir()?.join(format!("{name}.toml")))
        }
        None => {
            let cwd = env::current_dir()
                .map_err(|err| PackError::io("determine current directory", err))?;
            Ok(cwd.join(LOCAL_PROFILE))
        }
    }
}

impl RawProfile {
    fn into_profile(self, stem: &str, config_dir: &Path) -> Result<ServerProfile> {
        let name = match self.name {
            Some(name) if !name.trim().is_empty() => name,
            _ => stem.to_string(),
        };

        let server = ServerSection {
            root: resolve_against_config(config_dir, &self.server.root)?,
        };
        self.pack.validate()?;
        let api_base = match self.pack.api_base.as_deref().map(str::trim) {
            Some(base) if !base.is_empty() => Some(normalize_base(base)?),
            _ => None,
        };
        let pack = PackSection {
            api_base,
            optional_files: self.pack.optional_files,
            provider: self.pack.provider,
            project_id: self.pack.project_id,
            slug: self.pack.slug,
            archive: match &self.pack.archive {
                Some(path) => Some(resolve_against_config(config_dir, path)?),
                None if self.pack.provider == ProviderKind::Local => {
                    Some(server.root.join(DEFAULT_DROP_DIR))
                }
                None => None,
            },
        };
        let overlay = OverlaySection {
            path: resolve_against_config(config_dir, &self.overlay.path)?,
        };
        let controller = self.controller.into_section()?;

        Ok(ServerProfile {
            name,
            server,
            pack,
            overlay,
            controller,
            secrets: self.secrets,
        })
    }
}

impl RawPack {
    fn validate(&self) -> Result<()> {
        match self.provider {
            ProviderKind::CurseForge => {
                let numeric = self
                    .project_id
                    .as_deref()
                    .and_then(|id| id.parse::<u32>().ok())
                    .is_some_and(|id| id != 0);
                if numeric {
                    Ok(())
                } else {
                    Err(PackError::Config(
                        "pack provider 'curseforge' requires a numeric, non-zero 'project_id'"
                            .to_string(),
                    ))
                }
            }
            ProviderKind::Modrinth => {
                let has_slug = self
                    .slug
                    .as_deref()
                    .is_some_and(|slug| !slug.trim().is_empty());
                if self.project_id.is_some() || has_slug {
                    Ok(())
                } else {
                    Err(PackError::Config(
                        "pack provider 'modrinth' requires 'project_id' (or 'slug')".to_string(),
                    ))
                }
            }
            ProviderKind::Local => Ok(()),
        }
    }
}

impl RawController {
    fn into_section(self) -> Result<ControllerSection> {
        match self.kind {
            ControllerKind::Amp => {
                if self.instance.as_deref().unwrap_or("").is_empty() {
                    return Err(PackError::Config(
                        "controller type 'amp' requires a non-empty 'instance'".to_string(),
                    ));
                }
            }
            ControllerKind::Command => {
                if self.command.is_none() {
                    return Err(PackError::Config(
                        "controller type 'command' requires a [controller.command] section"
                            .to_string(),
                    ));
                }
            }
        }

        Ok(ControllerSection {
            kind: self.kind,
            instance: self.instance,
            command: self.command,
        })
    }
}

/// Resolve `path` against `config_dir` and make the result absolute without
/// requiring the path to exist on disk.
fn resolve_against_config(config_dir: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    };
    let normalized = lexically_normalize(&joined);
    if normalized.is_absolute() {
        Ok(normalized)
    } else {
        let cwd =
            env::current_dir().map_err(|err| PackError::io("determine current directory", err))?;
        Ok(cwd.join(normalized))
    }
}

/// A new server profile to write with [`write_profile`] or
/// [`write_local_profile`].
#[derive(Debug, Clone)]
pub struct ProfileDraft {
    pub name: String,
    pub server_root: PathBuf,
    pub provider: ProviderKind,
    /// The provider's project id; written as a TOML integer for CurseForge
    /// and as a string otherwise, and left out when `None`.
    pub project_id: Option<String>,
    /// Human-friendly pack identifier; written only when non-empty.
    pub slug: Option<String>,
    /// Local archive path (.zip or .mrpack file, or a directory of them) when the provider is
    /// `local`.
    pub archive: Option<PathBuf>,
    pub overlay_path: PathBuf,
    pub controller: ControllerKind,
    /// AMP instance name, required when `controller` is `Amp`.
    pub instance: Option<String>,
    /// Command config, required when `controller` is `Command`.
    pub command: Option<CommandConfig>,
    /// Encrypted secrets to store with the profile.
    pub secrets: Option<SecretsSection>,
    /// CurseForge API base override (`[pack] api_base`); left out when `None`.
    pub api_base: Option<String>,
    /// Modrinth optional server files; left out when it is the default.
    pub optional_files: OptionalFiles,
}

/// Writes a new profile `<name>.toml` into the profile directory.
///
/// Errors when the profile already exists unless `force` is set. Paths are
/// written as given; callers should write absolute paths so the profile does
/// not depend on where it is loaded from. Returns the written path.
pub fn write_profile(draft: &ProfileDraft, force: bool) -> Result<PathBuf> {
    validate_profile_name(&draft.name)?;

    let dir = profile_dir()?;
    let path = dir.join(format!("{}.toml", draft.name));
    if path.exists() && !force {
        return Err(PackError::Config(format!(
            "profile '{}' already exists at {} (use --force to overwrite)",
            draft.name,
            path.display()
        )));
    }

    fs::create_dir_all(&dir).map_err(|err| {
        PackError::io(format!("create profile directory '{}'", dir.display()), err)
    })?;

    write_profile_at(draft, &path)
}

/// Writes a profile as `.packctl.toml` inside `dir` (the server root).
///
/// The config file travels with the instance, and relative paths resolve
/// against its own directory, so `root = "."` and `overlay = "overlay"` are
/// the natural forms here.
pub fn write_local_profile(draft: &ProfileDraft, force: bool, dir: &Path) -> Result<PathBuf> {
    validate_profile_name(&draft.name)?;

    let path = dir.join(LOCAL_PROFILE);
    if path.exists() && !force {
        return Err(PackError::Config(format!(
            "{LOCAL_PROFILE} already exists at {} (use --force to overwrite)",
            path.display()
        )));
    }
    write_profile_at(draft, &path)
}

fn write_profile_at(draft: &ProfileDraft, path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| PackError::io(format!("create '{}'", parent.display()), err))?;
    }
    let content = toml::to_string_pretty(&profile_write_from_draft(draft))
        .map_err(|err| PackError::Other(format!("serialize profile: {err}")))?;
    atomic_write(path, content.as_bytes())?;
    Ok(path.to_path_buf())
}

fn profile_write_from_draft(draft: &ProfileDraft) -> ProfileWrite {
    ProfileWrite {
        name: Some(draft.name.clone()),
        server: ServerWrite {
            root: draft.server_root.clone(),
        },
        pack: PackWrite {
            provider: draft.provider,
            project_id: project_id_write(draft.provider, draft.project_id.as_deref()),
            slug: draft.slug.clone(),
            archive: draft.archive.clone(),
            api_base: draft.api_base.clone(),
            optional_files: draft.optional_files,
        },
        overlay: OverlayWrite {
            path: draft.overlay_path.clone(),
        },
        controller: ControllerWrite {
            kind: draft.controller,
            instance: draft.instance.clone(),
            command: draft.command.clone(),
        },
        secrets: draft.secrets.clone(),
    }
}

/// Stores or removes the encrypted API key blob in an existing profile file,
/// preserving every other field exactly as written.
///
/// The file is edited as TOML so relative paths and formatting choices made by
/// [`write_local_profile`] survive.
pub fn update_secret_in_file(path: &Path, api_key_blob: Option<&str>) -> Result<()> {
    let content = fs::read_to_string(path)
        .map_err(|err| PackError::io(format!("read '{}'", path.display()), err))?;
    let mut value: toml::Value =
        toml::from_str(&content).map_err(|err| PackError::Parse(err.to_string()))?;

    match api_key_blob {
        Some(blob) => {
            let table = value
                .as_table_mut()
                .ok_or_else(|| PackError::Config("profile is not a TOML table".to_string()))?;
            let secrets = table
                .entry("secrets".to_string())
                .or_insert_with(|| toml::Value::Table(Default::default()));
            let secrets = secrets
                .as_table_mut()
                .ok_or_else(|| PackError::Config("profile 'secrets' is not a table".to_string()))?;
            secrets.insert("api_key".to_string(), toml::Value::String(blob.to_string()));
        }
        None => {
            if let Some(secrets) = value
                .as_table_mut()
                .and_then(|table| table.get_mut("secrets"))
                .and_then(|secrets| secrets.as_table_mut())
            {
                secrets.remove("api_key");
            }
        }
    }

    let out = toml::to_string_pretty(&value)
        .map_err(|err| PackError::Other(format!("serialize profile: {err}")))?;
    atomic_write(path, out.as_bytes())?;
    Ok(())
}

/// Replace a file only after its complete new contents have been written to a
/// temporary sibling. Renaming within one directory is atomic on supported
/// filesystems, so readers see either complete version of a profile.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        PackError::Config(format!(
            "profile path '{}' has no parent directory",
            path.display()
        ))
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        PackError::Config(format!(
            "profile path '{}' has no file name",
            path.display()
        ))
    })?;

    for attempt in 0..100 {
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            attempt
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(mut file) => {
                let write_result = (|| -> io::Result<()> {
                    file.write_all(contents)?;
                    file.sync_all()
                })();
                if let Err(err) = write_result {
                    let _ = fs::remove_file(&temporary);
                    return Err(PackError::io(
                        format!("write profile '{}'", path.display()),
                        err,
                    ));
                }
                if let Err(err) = fs::rename(&temporary, path) {
                    let _ = fs::remove_file(&temporary);
                    return Err(PackError::io(
                        format!("replace profile '{}'", path.display()),
                        err,
                    ));
                }
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(PackError::io(
                    format!("create temporary profile for '{}'", path.display()),
                    err,
                ));
            }
        }
    }

    Err(PackError::Other(format!(
        "could not allocate a temporary file to update profile '{}'",
        path.display()
    )))
}

/// Rejects profile names that would escape the profile directory or name
/// another file.
fn validate_profile_name(name: &str) -> Result<()> {
    let invalid =
        name.trim().is_empty() || name == "." || name == ".." || name.contains(['/', '\\']);
    if invalid {
        return Err(PackError::Config(format!(
            "invalid profile name '{name}': must be a single path segment"
        )));
    }
    Ok(())
}

/// Serialization shape of a profile file. Mirrors the raw deserialization
/// shape so everything [`write_profile`] writes is readable again.
#[derive(Debug, Serialize)]
struct ProfileWrite {
    name: Option<String>,
    server: ServerWrite,
    pack: PackWrite,
    overlay: OverlayWrite,
    controller: ControllerWrite,
    #[serde(skip_serializing_if = "Option::is_none")]
    secrets: Option<SecretsSection>,
}

#[derive(Debug, Serialize)]
struct ServerWrite {
    root: PathBuf,
}

#[derive(Debug, Serialize)]
struct PackWrite {
    provider: ProviderKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<ProjectIdWrite>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_base: Option<String>,
    #[serde(skip_serializing_if = "OptionalFiles::is_default")]
    optional_files: OptionalFiles,
}

/// How a project id is written: CurseForge ids stay TOML integers so
/// profiles keep their historical shape; other ids are strings.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ProjectIdWrite {
    Int(u32),
    Str(String),
}

fn project_id_write(provider: ProviderKind, id: Option<&str>) -> Option<ProjectIdWrite> {
    let id = id.map(str::trim).filter(|id| !id.is_empty())?;
    match (provider, id.parse::<u32>()) {
        (ProviderKind::CurseForge, Ok(number)) => Some(ProjectIdWrite::Int(number)),
        _ => Some(ProjectIdWrite::Str(id.to_string())),
    }
}

#[derive(Debug, Serialize)]
struct OverlayWrite {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct ControllerWrite {
    #[serde(rename = "type")]
    kind: ControllerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<CommandConfig>,
}

/// Remove `.` components and resolve `..` components lexically, without
/// touching the filesystem.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(Component::ParentDir.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{EnvGuard, env_lock};

    const AMP_CONTROLLER: &str = "[controller]\ntype = \"amp\"\ninstance = \"x\"\n";

    fn base_toml(controller: &str) -> String {
        format!(
            "[server]\nroot = \"/srv/mc\"\n\n[pack]\nprovider = \"curseforge\"\nproject_id = 1\n\n[overlay]\npath = \"overlay\"\n\n{controller}"
        )
    }

    #[cfg(unix)]
    #[test]
    fn full_profile_parses() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = r#"
name = "ATM10"

[server]
root = "/home/amp/.ampdata/instances/ATM10/Minecraft"

[pack]
provider = "curseforge"
project_id = 925200
slug = "atm10"

[overlay]
path = "./overlay"

[controller]
type = "amp"
instance = "ATM10"
"#;
        fs::write(dir.path().join("atm10.toml"), content).unwrap();

        let profile = load_profile("atm10").unwrap();

        assert_eq!(profile.name, "ATM10");
        assert_eq!(
            profile.server.root,
            PathBuf::from("/home/amp/.ampdata/instances/ATM10/Minecraft")
        );
        assert_eq!(profile.pack.provider, ProviderKind::CurseForge);
        assert_eq!(profile.pack.project_id.as_deref(), Some("925200"));
        assert_eq!(profile.pack.slug.as_deref(), Some("atm10"));
        assert_eq!(profile.overlay.path, dir.path().join("overlay"));
        assert_eq!(profile.controller.kind, ControllerKind::Amp);
        assert_eq!(profile.controller.instance.as_deref(), Some("ATM10"));
        assert!(profile.controller.command.is_none());
    }

    #[test]
    fn missing_name_defaults_to_file_stem() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        fs::write(dir.path().join("my-server.toml"), base_toml(AMP_CONTROLLER)).unwrap();

        let profile = load_profile("my-server").unwrap();
        assert_eq!(profile.name, "my-server");
    }

    #[test]
    fn local_profile_parses_archive_relative_to_config_dir() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = r#"
[server]
root = "/srv/mc"

[pack]
provider = "local"
archive = "./packs/server-pack.zip"

[overlay]
path = "overlay"

[controller]
type = "amp"
instance = "x"
"#;
        fs::write(dir.path().join("sb4.toml"), content).unwrap();

        let profile = load_profile("sb4").unwrap();
        assert_eq!(profile.pack.provider, ProviderKind::Local);
        assert_eq!(profile.pack.project_id, None);
        assert_eq!(
            profile.pack.archive.as_deref(),
            Some(dir.path().join("packs/server-pack.zip").as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_profile_without_archive_defaults_to_packs_drop_folder() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = "[server]\nroot = \"/srv/mc\"\n\n[pack]\nprovider = \"local\"\n\n[overlay]\npath = \"overlay\"\n\n"
            .to_string()
            + AMP_CONTROLLER;
        fs::write(dir.path().join("drop.toml"), content).unwrap();

        let profile = load_profile("drop").unwrap();
        assert_eq!(profile.pack.provider, ProviderKind::Local);
        assert_eq!(
            profile.pack.archive.as_deref(),
            Some(Path::new("/srv/mc/packs"))
        );
    }

    #[test]
    fn curseforge_profile_without_project_id_errors() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = "[server]\nroot = \"/srv/mc\"\n\n[pack]\nprovider = \"curseforge\"\n\n[overlay]\npath = \"overlay\"\n\n"
            .to_string()
            + AMP_CONTROLLER;
        fs::write(dir.path().join("bad.toml"), content).unwrap();

        let err = load_profile("bad").unwrap_err();
        assert!(
            matches!(&err, PackError::Config(message) if message.contains("project_id")),
            "expected Config error, got {err:?}"
        );
    }

    #[test]
    fn unknown_provider_errors() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = "[server]\nroot = \"/srv/mc\"\n\n[pack]\nprovider = \"ftb\"\nproject_id = 1\n\n[overlay]\npath = \"overlay\"\n\n"
            .to_string()
            + AMP_CONTROLLER;
        fs::write(dir.path().join("bad.toml"), content).unwrap();

        let err = load_profile("bad").unwrap_err();
        assert!(
            matches!(err, PackError::Parse(_)),
            "expected Parse error, got {err:?}"
        );
    }

    /// Loads a profile whose `[pack]` section is `pack` (a TOML body).
    fn load_with_pack(pack: &str) -> Result<ServerProfile> {
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());
        let content = format!(
            "[server]\nroot = \"/srv/mc\"\n\n[pack]\n{pack}\n\n[overlay]\npath = \"overlay\"\n\n{AMP_CONTROLLER}"
        );
        fs::write(dir.path().join("p.toml"), content).unwrap();
        load_profile("p")
    }

    fn curseforge_draft(project_id: &str) -> ProfileDraft {
        ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: Some(project_id.to_string()),
            slug: Some("all-the-mods-10".to_string()),
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("ATM10".to_string()),
            command: None,
            secrets: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        }
    }

    #[test]
    fn integer_project_id_still_loads_and_round_trips_as_integer() {
        let _lock = env_lock();
        let profile = load_with_pack("provider = \"curseforge\"\nproject_id = 925200").unwrap();
        assert_eq!(profile.pack.project_id.as_deref(), Some("925200"));

        let dir = tempfile::tempdir().unwrap();
        let draft = ProfileDraft {
            project_id: profile.pack.project_id.clone(),
            ..curseforge_draft("ignored")
        };
        let path = write_local_profile(&draft, false, dir.path()).unwrap();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("project_id = 925200\n"), "{content}");
        assert!(!content.contains("\"925200\""), "{content}");
    }

    #[test]
    fn optional_files_round_trip_and_default_not_written() {
        let _lock = env_lock();
        let profile = load_with_pack("provider = \"modrinth\"\nproject_id = \"5FFgwNNP\"").unwrap();
        assert_eq!(profile.pack.optional_files, OptionalFiles::Include);
        let profile = load_with_pack(
            "provider = \"modrinth\"\nproject_id = \"5FFgwNNP\"\noptional_files = \"exclude\"",
        )
        .unwrap();
        assert_eq!(profile.pack.optional_files, OptionalFiles::Exclude);
        assert!(matches!(
            load_with_pack(
                "provider = \"modrinth\"\nproject_id = \"5FFgwNNP\"\noptional_files = \"maybe\""
            ),
            Err(PackError::Parse(_))
        ));

        let dir = tempfile::tempdir().unwrap();
        let draft = ProfileDraft {
            provider: ProviderKind::Modrinth,
            project_id: Some("5FFgwNNP".to_string()),
            ..curseforge_draft("1")
        };
        let path = write_local_profile(&draft, true, dir.path()).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("optional_files"), "{content}");

        let draft = ProfileDraft {
            optional_files: OptionalFiles::Exclude,
            ..draft
        };
        write_local_profile(&draft, true, dir.path()).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("optional_files = \"exclude\""),
            "{content}"
        );
        let loaded = load_local_profile(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.pack.optional_files, OptionalFiles::Exclude);
    }

    #[test]
    fn string_project_id_loads_for_modrinth() {
        let _lock = env_lock();
        let profile =
            load_with_pack("provider = \"modrinth\"\nproject_id = \" 1KVo5zza \"").unwrap();
        assert_eq!(profile.pack.provider, ProviderKind::Modrinth);
        assert_eq!(profile.pack.project_id.as_deref(), Some("1KVo5zza"));

        // Written back as a string, never as a number, even when it looks
        // numeric.
        let dir = tempfile::tempdir().unwrap();
        for (id, expected) in [
            ("1KVo5zza", "project_id = \"1KVo5zza\""),
            ("123", "project_id = \"123\""),
        ] {
            let draft = ProfileDraft {
                provider: ProviderKind::Modrinth,
                project_id: Some(id.to_string()),
                ..curseforge_draft("1")
            };
            let path = write_local_profile(&draft, true, dir.path()).unwrap();
            let content = fs::read_to_string(&path).unwrap();
            assert!(content.contains(expected), "{content}");
            let loaded = load_local_profile(dir.path()).unwrap().unwrap();
            assert_eq!(loaded.pack.project_id.as_deref(), Some(id));
        }
    }

    #[test]
    fn curseforge_string_numeric_id_accepted() {
        let _lock = env_lock();
        let profile = load_with_pack("provider = \"curseforge\"\nproject_id = \"925200\"").unwrap();
        assert_eq!(profile.pack.project_id.as_deref(), Some("925200"));
    }

    #[test]
    fn curseforge_non_numeric_id_rejected() {
        let _lock = env_lock();
        for pack in [
            "provider = \"curseforge\"\nproject_id = \"atm10\"",
            "provider = \"curseforge\"\nproject_id = 0",
            "provider = \"curseforge\"\nproject_id = \"\"",
            "provider = \"curseforge\"\nproject_id = 99999999999",
            "provider = \"curseforge\"",
        ] {
            let err = load_with_pack(pack).unwrap_err();
            assert!(
                matches!(&err, PackError::Config(m)
                    if m == "pack provider 'curseforge' requires a numeric, non-zero 'project_id'"),
                "{pack}: {err:?}"
            );
        }
    }

    #[test]
    fn modrinth_requires_project_id_or_slug() {
        let _lock = env_lock();
        for pack in [
            "provider = \"modrinth\"",
            "provider = \"modrinth\"\nslug = \"  \"",
            "provider = \"modrinth\"\nproject_id = \"\"",
        ] {
            let err = load_with_pack(pack).unwrap_err();
            assert!(
                matches!(&err, PackError::Config(m)
                    if m == "pack provider 'modrinth' requires 'project_id' (or 'slug')"),
                "{pack}: {err:?}"
            );
        }
        let by_slug =
            load_with_pack("provider = \"modrinth\"\nslug = \"fabulously-optimized\"").unwrap();
        assert_eq!(by_slug.pack.project_id, None);
        assert_eq!(by_slug.pack.slug.as_deref(), Some("fabulously-optimized"));
    }

    #[test]
    fn written_curseforge_profile_byte_identical_to_previous_format() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_local_profile(&curseforge_draft("925200"), false, dir.path()).unwrap();
        let content = fs::read_to_string(path).unwrap();
        // The exact shape packctl wrote while project ids were integers.
        assert_eq!(
            content,
            "name = \"atm10\"\n\n[server]\nroot = \"/srv/atm10\"\n\n[pack]\nprovider = \"curseforge\"\nproject_id = 925200\nslug = \"all-the-mods-10\"\n\n[overlay]\npath = \"/srv/atm10/overlay\"\n\n[controller]\ntype = \"amp\"\ninstance = \"ATM10\"\n"
        );
    }

    #[test]
    fn api_base_loads_and_validates() {
        let _lock = env_lock();
        let profile = load_with_pack(
            "provider = \"curseforge\"\nproject_id = 925200\napi_base = \"https://cf.example/v1/\"",
        )
        .unwrap();
        assert_eq!(profile.pack.api_base.as_deref(), Some("https://cf.example"));

        let profile =
            load_with_pack("provider = \"curseforge\"\nproject_id = 925200\napi_base = \"  \"")
                .unwrap();
        assert_eq!(profile.pack.api_base, None);

        for bad in [
            "http://cf.example",
            "https://api.curse.tools/v1/cf",
            "https://user:pw@cf.example",
        ] {
            let err = load_with_pack(&format!(
                "provider = \"curseforge\"\nproject_id = 925200\napi_base = \"{bad}\""
            ))
            .unwrap_err();
            assert!(
                matches!(&err, PackError::Config(m) if m.starts_with("invalid CurseForge API base")),
                "{bad}: {err:?}"
            );
        }

        // A written api_base round-trips; absent, nothing is written.
        let dir = tempfile::tempdir().unwrap();
        let draft = ProfileDraft {
            api_base: Some("https://cf.example".to_string()),
            optional_files: OptionalFiles::default(),
            ..curseforge_draft("925200")
        };
        let path = write_local_profile(&draft, false, dir.path()).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("api_base = \"https://cf.example\""),
            "{content}"
        );
        let loaded = load_local_profile(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.pack.api_base.as_deref(), Some("https://cf.example"));
    }

    #[test]
    fn curseforge_api_base_env_overrides_profile() {
        use crate::providers::curseforge::endpoint::{CfEndpointMode, ENV_API_BASE};

        let _lock = env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("PACKCTL_HOME", home.path().as_os_str());
        let _key = EnvGuard::set("CF_API_KEY", std::ffi::OsStr::new(""));
        let _base = EnvGuard::set(ENV_API_BASE, std::ffi::OsStr::new(""));

        let mut profile = load_with_pack(
            "provider = \"curseforge\"\nproject_id = 925200\napi_base = \"https://profile.example\"",
        )
        .unwrap();
        assert_eq!(
            profile.curseforge_api_base().as_deref(),
            Some("https://profile.example")
        );
        assert_eq!(
            profile.curseforge_endpoint().unwrap().base_url,
            "https://profile.example"
        );

        let _env = EnvGuard::set(ENV_API_BASE, std::ffi::OsStr::new("https://env.example/v1"));
        assert_eq!(
            profile.curseforge_api_base().as_deref(),
            Some("https://env.example/v1")
        );
        let endpoint = profile.curseforge_endpoint().unwrap();
        assert_eq!(endpoint.base_url, "https://env.example");
        assert_eq!(endpoint.mode, CfEndpointMode::Custom);

        // Neither set: no key means the community proxy.
        let _env = EnvGuard::set(ENV_API_BASE, std::ffi::OsStr::new(""));
        profile.pack.api_base = None;
        assert_eq!(profile.curseforge_api_base(), None);
        assert_eq!(
            profile.curseforge_endpoint().unwrap().mode,
            CfEndpointMode::Proxy
        );
    }

    #[test]
    fn update_secret_in_file_preserves_integer_project_id() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_local_profile(&curseforge_draft("925200"), false, dir.path()).unwrap();

        update_secret_in_file(&path, Some("v1:c2VjcmV0")).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("project_id = 925200\n"), "{content}");
        let profile = load_local_profile(dir.path()).unwrap().unwrap();
        assert_eq!(profile.pack.project_id.as_deref(), Some("925200"));
        assert_eq!(profile.secrets.api_key.as_deref(), Some("v1:c2VjcmV0"));
    }

    #[test]
    fn provider_kind_names_match_profile_spelling() {
        for kind in [
            ProviderKind::CurseForge,
            ProviderKind::Modrinth,
            ProviderKind::Local,
        ] {
            let serialized = toml::Value::try_from(kind).unwrap();
            assert_eq!(serialized.as_str(), Some(kind.as_str()));
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn amp_controller_without_instance_errors() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = base_toml("[controller]\ntype = \"amp\"\n");
        fs::write(dir.path().join("bad.toml"), content).unwrap();

        let err = load_profile("bad").unwrap_err();
        assert!(
            matches!(err, PackError::Config(_)),
            "expected Config error, got {err:?}"
        );
    }

    #[test]
    fn command_controller_without_command_section_errors() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = base_toml("[controller]\ntype = \"command\"\n");
        fs::write(dir.path().join("bad.toml"), content).unwrap();

        let err = load_profile("bad").unwrap_err();
        assert!(
            matches!(err, PackError::Config(_)),
            "expected Config error, got {err:?}"
        );
    }

    #[test]
    fn missing_profile_returns_not_found() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let err = load_profile("nope").unwrap_err();
        match err {
            PackError::NotFound(message) => {
                assert!(message.contains("nope"), "message: {message}");
                assert!(
                    message.contains(&dir.path().display().to_string()),
                    "message: {message}"
                );
            }
            other => panic!("expected NotFound error, got {other:?}"),
        }
    }

    #[test]
    fn named_profile_routes_reject_unsafe_names() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        for name in [
            "",
            ".",
            "..",
            "../escape",
            "nested/profile",
            r"nested\profile",
        ] {
            let load_error = load_profile(name).unwrap_err();
            assert!(matches!(load_error, PackError::Config(_)), "name: {name}");

            let path_error = profile_file_path(Some(name)).unwrap_err();
            assert!(matches!(path_error, PackError::Config(_)), "name: {name}");
        }
    }

    #[test]
    fn xdg_config_home_is_used_even_before_directory_exists() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let xdg = dir.path().join("xdg-does-not-exist");
        let _xdg_guard = EnvGuard::set("XDG_CONFIG_HOME", xdg.as_os_str());
        let _home_guard = EnvGuard::set("HOME", dir.path().as_os_str());
        let previous_packctl_home = env::var_os("PACKCTL_HOME");
        // PACKCTL_HOME is inherited by other tests, so explicitly remove it.
        unsafe { env::remove_var("PACKCTL_HOME") };

        assert_eq!(profile_dir().unwrap(), xdg.join("packctl"));

        match previous_packctl_home {
            Some(value) => unsafe { env::set_var("PACKCTL_HOME", value) },
            None => unsafe { env::remove_var("PACKCTL_HOME") },
        }
    }

    #[test]
    fn list_profiles_finds_and_sorts() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let template = |name: &str| format!("name = \"{name}\"\n{}", base_toml(AMP_CONTROLLER));
        fs::write(dir.path().join("zzz.toml"), template("aaa")).unwrap();
        fs::write(dir.path().join("mmm.toml"), template("mmm")).unwrap();

        let profiles = list_profiles().unwrap();
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["aaa", "mmm"]);
    }

    #[test]
    fn list_profiles_returns_empty_when_directory_missing() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let _guard = EnvGuard::set("PACKCTL_HOME", missing.as_os_str());

        let profiles = list_profiles().unwrap();
        assert!(profiles.is_empty());
    }

    #[test]
    fn unparseable_toml_returns_parse_error() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        fs::write(dir.path().join("broken.toml"), "not [ valid toml = ==").unwrap();

        let err = load_profile("broken").unwrap_err();
        assert!(
            matches!(err, PackError::Parse(_)),
            "expected Parse error, got {err:?}"
        );
    }

    #[test]
    fn relative_paths_resolve_against_config_dir() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = "[server]\nroot = \"./Minecraft\"\n\n[pack]\nprovider = \"curseforge\"\nproject_id = 1\n\n[overlay]\npath = \"../shared-overlay\"\n\n"
            .to_string()
            + AMP_CONTROLLER;
        fs::write(dir.path().join("rel.toml"), content).unwrap();

        let profile = load_profile("rel").unwrap();
        assert_eq!(profile.server.root, dir.path().join("Minecraft"));
        assert_eq!(
            profile.overlay.path,
            dir.path().parent().unwrap().join("shared-overlay")
        );
    }

    #[test]
    fn command_controller_parses() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let content = r#"
[server]
root = "/srv/mc"

[pack]
provider = "curseforge"
project_id = 1

[overlay]
path = "overlay"

[controller]
type = "command"

[controller.command]
status = ["pgrep", "-f", "server.jar"]
stop = ["screen", "-S", "mc", "-X", "stuff", "stop\n"]
start = ["systemctl", "start", "mc"]
timeout_ms = 30000
"#;
        fs::write(dir.path().join("cmd.toml"), content).unwrap();

        let profile = load_profile("cmd").unwrap();
        assert_eq!(profile.controller.kind, ControllerKind::Command);
        assert!(profile.controller.instance.is_none());

        let command = profile.controller.command.as_ref().unwrap();
        assert_eq!(command.status.join(" "), "pgrep -f server.jar");
        assert_eq!(command.stop.join(" "), "screen -S mc -X stuff stop\n");
        assert_eq!(command.start.join(" "), "systemctl start mc");
        assert_eq!(command.timeout_ms, Some(30000));
    }

    #[cfg(unix)]
    #[test]
    fn write_profile_round_trips_amp() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: Some("925200".to_string()),
            slug: Some("all-the-mods-10".to_string()),
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("ATM10".to_string()),
            command: None,
            secrets: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        };

        let path = write_profile(&draft, false).unwrap();
        assert_eq!(path, dir.path().join("atm10.toml"));

        let profile = load_profile("atm10").unwrap();
        assert_eq!(profile.name, "atm10");
        assert_eq!(profile.server.root, PathBuf::from("/srv/atm10"));
        assert_eq!(profile.pack.provider, ProviderKind::CurseForge);
        assert_eq!(profile.pack.project_id.as_deref(), Some("925200"));
        assert_eq!(profile.pack.slug.as_deref(), Some("all-the-mods-10"));
        assert_eq!(profile.overlay.path, PathBuf::from("/srv/atm10/overlay"));
        assert_eq!(profile.controller.kind, ControllerKind::Amp);
        assert_eq!(profile.controller.instance.as_deref(), Some("ATM10"));
        assert!(profile.controller.command.is_none());
    }

    #[test]
    fn write_profile_round_trips_command() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "mc".to_string(),
            server_root: PathBuf::from("/srv/mc"),
            provider: ProviderKind::CurseForge,
            project_id: Some("1".to_string()),
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/mc/overlay"),
            controller: ControllerKind::Command,
            instance: None,
            command: Some(CommandConfig {
                status: vec![
                    "pgrep".to_string(),
                    "-f".to_string(),
                    "server.jar".to_string(),
                ],
                stop: vec![
                    "screen".to_string(),
                    "-S".to_string(),
                    "mc".to_string(),
                    "-X".to_string(),
                    "stuff".to_string(),
                    "stop\n".to_string(),
                ],
                start: vec![
                    "systemctl".to_string(),
                    "start".to_string(),
                    "mc".to_string(),
                ],
                timeout_ms: Some(30000),
            }),
            secrets: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        };

        write_profile(&draft, false).unwrap();

        let profile = load_profile("mc").unwrap();
        let command = profile.controller.command.as_ref().unwrap();
        assert_eq!(command.status.join(" "), "pgrep -f server.jar");
        assert_eq!(command.stop.join(" "), "screen -S mc -X stuff stop\n");
        assert_eq!(command.start.join(" "), "systemctl start mc");
        assert_eq!(command.timeout_ms, Some(30000));
    }

    #[test]
    fn write_profile_refuses_overwrite_without_force() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: Some("1".to_string()),
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("ATM10".to_string()),
            command: None,
            secrets: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        };

        write_profile(&draft, false).unwrap();

        let err = write_profile(&draft, false).unwrap_err();
        assert!(
            matches!(err, PackError::Config(_)),
            "expected Config error, got {err:?}"
        );

        write_profile(&draft, true).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_profile_round_trips_local() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "sb4".to_string(),
            server_root: PathBuf::from("/srv/sb4"),
            provider: ProviderKind::Local,
            project_id: None,
            slug: None,
            archive: Some(PathBuf::from("/packs/server-pack.zip")),
            overlay_path: PathBuf::from("/srv/sb4/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("Stoneblock401".to_string()),
            command: None,
            secrets: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        };

        let path = write_profile(&draft, false).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("provider = \"local\""),
            "content: {content}"
        );
        assert!(
            content.contains("archive = \"/packs/server-pack.zip\""),
            "content: {content}"
        );
        assert!(!content.contains("project_id"), "content: {content}");

        let profile = load_profile("sb4").unwrap();
        assert_eq!(profile.pack.provider, ProviderKind::Local);
        assert_eq!(
            profile.pack.archive.as_deref(),
            Some(Path::new("/packs/server-pack.zip"))
        );
    }

    #[test]
    fn write_profile_rejects_unsafe_names() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "../escape".to_string(),
            server_root: PathBuf::from("/srv"),
            provider: ProviderKind::CurseForge,
            project_id: Some("1".to_string()),
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("x".to_string()),
            command: None,
            secrets: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        };

        let err = write_profile(&draft, false).unwrap_err();
        assert!(
            matches!(err, PackError::Config(_)),
            "expected Config error, got {err:?}"
        );
        assert!(!dir.path().join("..").join("escape.toml").exists());
    }

    #[test]
    fn local_profile_writes_and_round_trips() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "my-server".to_string(),
            server_root: PathBuf::from("."),
            provider: ProviderKind::CurseForge,
            project_id: Some("925200".to_string()),
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("overlay"),
            controller: ControllerKind::Amp,
            instance: Some("my-server".to_string()),
            command: None,
            secrets: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        };

        let path = write_local_profile(&draft, false, dir.path()).unwrap();
        assert_eq!(path, dir.path().join(LOCAL_PROFILE));

        let profile = load_local_profile(dir.path()).unwrap().unwrap();
        assert_eq!(profile.name, "my-server");
        assert_eq!(profile.server.root, dir.path());
        assert_eq!(profile.overlay.path, dir.path().join("overlay"));
        assert_eq!(profile.controller.kind, ControllerKind::Amp);
        assert_eq!(profile.controller.instance.as_deref(), Some("my-server"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_profile_uses_local_file_without_server_name() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        fs::write(dir.path().join(LOCAL_PROFILE), base_toml(AMP_CONTROLLER)).unwrap();

        let profile = resolve_local_profile_in(dir.path()).unwrap();
        let expected = dir
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap();
        assert_eq!(profile.name, expected);
        assert_eq!(profile.server.root, PathBuf::from("/srv/mc"));
    }

    #[test]
    fn missing_named_profile_hints_at_to_when_a_local_profile_exists() {
        let dir = tempfile::tempdir().unwrap();
        let missing = || PackError::NotFound("profile '1.2.0' not found in x".to_string());

        let err = with_local_profile_hint(missing(), "1.2.0", dir.path());
        assert_eq!(err.to_string(), "profile '1.2.0' not found in x");

        fs::write(dir.path().join(LOCAL_PROFILE), base_toml(AMP_CONTROLLER)).unwrap();
        let err = with_local_profile_hint(missing(), "1.2.0", dir.path());
        assert!(matches!(err, PackError::NotFound(_)));
        assert_eq!(
            err.to_string(),
            "profile '1.2.0' not found in x\nhint: '1.2.0' was read as a profile name; to \
             target a version of the local profile use --to 1.2.0"
        );

        // Other errors pass through unchanged.
        let err = with_local_profile_hint(PackError::Parse("bad".into()), "x", dir.path());
        assert!(matches!(err, PackError::Parse(_)));
    }

    #[test]
    fn resolve_profile_errors_without_local_file_or_name() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let err = resolve_local_profile_in(dir.path()).unwrap_err();
        assert!(
            matches!(err, PackError::NotFound(_)),
            "expected NotFound error, got {err:?}"
        );
    }

    #[test]
    fn update_secret_in_file_preserves_and_removes() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        fs::write(
            dir.path().join(LOCAL_PROFILE),
            "[server]\nroot = \".\"\n\n[pack]\nprovider = \"curseforge\"\nproject_id = 1\n\n[overlay]\npath = \"overlay\"\n\n[controller]\ntype = \"amp\"\ninstance = \"x\"\n",
        )
        .unwrap();

        let path = dir.path().join(LOCAL_PROFILE);
        update_secret_in_file(&path, Some("v1:c2VjcmV0")).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("[secrets]"), "content: {content}");
        assert!(
            content.contains("api_key = \"v1:c2VjcmV0\""),
            "content: {content}"
        );
        assert!(content.contains("project_id = 1"), "content: {content}");

        update_secret_in_file(&path, None).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("api_key"), "content: {content}");
        assert!(content.contains("project_id = 1"), "content: {content}");
    }

    #[test]
    fn profile_updates_leave_no_partial_or_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.toml");
        fs::write(&path, "old").unwrap();

        atomic_write(&path, b"new complete profile").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new complete profile");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn curseforge_api_key_prefers_env_over_stored() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: Some("1".to_string()),
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("x".to_string()),
            command: None,
            secrets: Some(SecretsSection {
                api_key: Some(crate::config::secrets::encrypt_string("stored-key").unwrap()),
            }),
            api_base: None,
            optional_files: OptionalFiles::default(),
        };
        write_profile(&draft, false).unwrap();

        let profile = load_profile("atm10").unwrap();
        assert_eq!(
            profile.curseforge_api_key().unwrap().as_deref(),
            Some("stored-key")
        );

        let _key_guard = EnvGuard::set("CF_API_KEY", std::ffi::OsStr::new("env-key"));
        assert_eq!(
            profile.curseforge_api_key().unwrap().as_deref(),
            Some("env-key")
        );
    }

    #[cfg(unix)]
    #[test]
    fn curseforge_api_key_falls_back_to_shared_global_key() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: Some("1".to_string()),
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("x".to_string()),
            command: None,
            secrets: None,
            api_base: None,
            optional_files: OptionalFiles::default(),
        };
        write_profile(&draft, false).unwrap();
        let profile = load_profile("atm10").unwrap();

        // No key anywhere yet.
        assert_eq!(profile.curseforge_api_key().unwrap(), None);

        // A shared global key covers a profile without its own key.
        crate::config::secrets::store_global_key("global-key").unwrap();
        assert_eq!(
            profile.curseforge_api_key().unwrap().as_deref(),
            Some("global-key")
        );

        // A per-profile key wins over the shared key.
        let with_own = ServerProfile {
            secrets: SecretsSection {
                api_key: Some(crate::config::secrets::encrypt_string("profile-key").unwrap()),
            },
            ..profile
        };
        assert_eq!(
            with_own.curseforge_api_key().unwrap().as_deref(),
            Some("profile-key")
        );

        // The environment still wins over everything.
        let _key_guard = EnvGuard::set("CF_API_KEY", std::ffi::OsStr::new("env-key"));
        assert_eq!(
            with_own.curseforge_api_key().unwrap().as_deref(),
            Some("env-key")
        );
    }
    #[test]
    fn table_rendering_pads_columns() {
        let rows = [
            [
                "atm10".to_string(),
                "curseforge".to_string(),
                "925200".to_string(),
                "/srv/atm10".to_string(),
            ],
            [
                "cobblemon-server".to_string(),
                "modrinth".to_string(),
                "cobblemon-modpack".to_string(),
                "/srv/cobble".to_string(),
            ],
        ];
        assert_eq!(
            profile_table(&rows),
            "NAME              PROVIDER    PROJECT            SERVER ROOT\n\
             atm10             curseforge  925200             /srv/atm10\n\
             cobblemon-server  modrinth    cobblemon-modpack  /srv/cobble\n"
        );
        // Headers set the minimum width.
        let short = [[
            "a".to_string(),
            "local".to_string(),
            "-".to_string(),
            "/r".to_string(),
        ]];
        assert_eq!(
            profile_table(&short),
            "NAME  PROVIDER  PROJECT  SERVER ROOT\na     local     -        /r\n"
        );
    }

    #[test]
    fn profile_project_prefers_id_then_slug() {
        let _lock = env_lock();
        let by_id =
            load_with_pack("provider = \"curseforge\"\nproject_id = 925200\nslug = \"atm10\"")
                .unwrap();
        assert_eq!(profile_project(&by_id), "925200");
        let by_slug =
            load_with_pack("provider = \"modrinth\"\nslug = \"fabulously-optimized\"").unwrap();
        assert_eq!(profile_project(&by_slug), "fabulously-optimized");
        let local = load_with_pack("provider = \"local\"").unwrap();
        assert_eq!(profile_project(&local), "-");
    }

    #[test]
    fn local_profile_line_names_the_local_profile() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(local_profile_line(dir.path()), None);
        write_local_profile(&curseforge_draft("925200"), false, dir.path()).unwrap();
        assert_eq!(
            local_profile_line(dir.path()).as_deref(),
            Some("Local profile here: atm10 (curseforge 925200)")
        );
        fs::write(dir.path().join(LOCAL_PROFILE), "not toml [").unwrap();
        let broken = local_profile_line(dir.path()).unwrap();
        assert!(
            broken.starts_with("Local profile here: .packctl.toml could not be loaded"),
            "{broken}"
        );
    }

    #[test]
    fn every_example_profile_loads() {
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut loaded = Vec::new();
        for entry in fs::read_dir(&examples).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            fs::copy(&path, dir.path().join(LOCAL_PROFILE)).unwrap();
            let profile = load_local_profile(dir.path())
                .unwrap_or_else(|err| panic!("{} does not load: {err}", path.display()))
                .unwrap();
            loaded.push((
                path.file_stem().unwrap().to_string_lossy().into_owned(),
                profile.pack.provider,
            ));
        }
        loaded.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            loaded,
            [
                ("atm10".to_string(), ProviderKind::CurseForge),
                ("local".to_string(), ProviderKind::Local),
                ("modrinth".to_string(), ProviderKind::Modrinth),
            ]
        );
    }
}
