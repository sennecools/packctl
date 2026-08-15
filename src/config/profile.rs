//! Server profile configuration loading.
//!
//! A profile is one TOML file per server, named `<name>.toml`, stored in the
//! packctl profile directory. See design notes "Configuration Model" and
//! "Terminology" for the domain model.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{PackError, Result};

/// Pack provider for a profile's upstream pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    CurseForge,
    Local,
}

/// Kind of server controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControllerKind {
    Amp,
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
    pub project_id: u32,
    pub slug: Option<String>,
    /// Local archive path (zip file or directory of zips) for the `local`
    /// provider. When omitted, `local` packs update from `<server root>/packs`.
    pub archive: Option<PathBuf>,
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
    /// The CurseForge API key to use for this profile.
    ///
    /// `$CF_API_KEY` wins when set; otherwise the profile's stored, encrypted
    /// key is decrypted; otherwise the shared, machine-wide key stored with
    /// `packctl apikey --global` is used. Returns `Ok(None)` when no key is
    /// available.
    pub fn curseforge_api_key(&self) -> Result<Option<String>> {
        let from_env = std::env::var("CF_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty());
        if let Some(key) = from_env {
            return Ok(Some(key));
        }
        if let Some(blob) = &self.secrets.api_key {
            return crate::config::secrets::decrypt_string(blob).map(Some);
        }
        crate::config::secrets::load_global_key()
    }
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
    #[serde(default)]
    project_id: u32,
    slug: Option<String>,
    #[serde(default)]
    archive: Option<PathBuf>,
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
pub fn list_cli() -> Result<()> {
    let profiles = list_profiles()?;
    if profiles.is_empty() {
        println!("No server profiles configured.");
        println!("Create a profile in {}", profile_dir()?.display());
        return Ok(());
    }
    println!("Server profiles");
    for profile in &profiles {
        println!("  {}", profile.name);
    }
    Ok(())
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
        return load_profile(name);
    }
    let cwd =
        env::current_dir().map_err(|err| PackError::io("determine current directory", err))?;
    resolve_local_profile_in(&cwd)
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
        let pack = PackSection {
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
            ProviderKind::CurseForge if self.project_id == 0 => Err(PackError::Config(
                "pack provider 'curseforge' requires a non-zero 'project_id'".to_string(),
            )),
            _ => Ok(()),
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
    pub project_id: u32,
    /// Human-friendly pack identifier; written only when non-empty.
    pub slug: Option<String>,
    /// Local archive path (zip file or directory of zips) when the provider is
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
            project_id: draft.project_id,
            slug: draft.slug.clone(),
            archive: draft.archive.clone(),
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
    #[serde(skip_serializing_if = "is_zero")]
    project_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive: Option<PathBuf>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
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
        assert_eq!(profile.pack.project_id, 925200);
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
        assert_eq!(profile.pack.project_id, 0);
        assert_eq!(
            profile.pack.archive.as_deref(),
            Some(dir.path().join("packs/server-pack.zip").as_path())
        );
    }

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

        let content = "[server]\nroot = \"/srv/mc\"\n\n[pack]\nprovider = \"modrinth\"\nproject_id = 1\n\n[overlay]\npath = \"overlay\"\n\n"
            .to_string()
            + AMP_CONTROLLER;
        fs::write(dir.path().join("bad.toml"), content).unwrap();

        let err = load_profile("bad").unwrap_err();
        assert!(
            matches!(err, PackError::Parse(_)),
            "expected Parse error, got {err:?}"
        );
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

    #[test]
    fn write_profile_round_trips_amp() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: 925200,
            slug: Some("all-the-mods-10".to_string()),
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("ATM10".to_string()),
            command: None,
            secrets: None,
        };

        let path = write_profile(&draft, false).unwrap();
        assert_eq!(path, dir.path().join("atm10.toml"));

        let profile = load_profile("atm10").unwrap();
        assert_eq!(profile.name, "atm10");
        assert_eq!(profile.server.root, PathBuf::from("/srv/atm10"));
        assert_eq!(profile.pack.provider, ProviderKind::CurseForge);
        assert_eq!(profile.pack.project_id, 925200);
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
            project_id: 1,
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
            project_id: 1,
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("ATM10".to_string()),
            command: None,
            secrets: None,
        };

        write_profile(&draft, false).unwrap();

        let err = write_profile(&draft, false).unwrap_err();
        assert!(
            matches!(err, PackError::Config(_)),
            "expected Config error, got {err:?}"
        );

        write_profile(&draft, true).unwrap();
    }

    #[test]
    fn write_profile_round_trips_local() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "sb4".to_string(),
            server_root: PathBuf::from("/srv/sb4"),
            provider: ProviderKind::Local,
            project_id: 0,
            slug: None,
            archive: Some(PathBuf::from("/packs/server-pack.zip")),
            overlay_path: PathBuf::from("/srv/sb4/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("Stoneblock401".to_string()),
            command: None,
            secrets: None,
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
            project_id: 1,
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("x".to_string()),
            command: None,
            secrets: None,
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
            project_id: 925200,
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("overlay"),
            controller: ControllerKind::Amp,
            instance: Some("my-server".to_string()),
            command: None,
            secrets: None,
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

    #[test]
    fn curseforge_api_key_prefers_env_over_stored() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: 1,
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("x".to_string()),
            command: None,
            secrets: Some(SecretsSection {
                api_key: Some(crate::config::secrets::encrypt_string("stored-key").unwrap()),
            }),
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

    #[test]
    fn curseforge_api_key_falls_back_to_shared_global_key() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let draft = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: 1,
            slug: None,
            archive: None,
            overlay_path: PathBuf::from("/srv/atm10/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("x".to_string()),
            command: None,
            secrets: None,
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
}
