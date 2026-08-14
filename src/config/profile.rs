//! Server profile configuration loading.
//!
//! A profile is one TOML file per server, named `<name>.toml`, stored in the
//! packctl profile directory. See design notes "Configuration Model" and
//! "Terminology" for the domain model.

use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::error::{PackError, Result};

/// Pack provider for a profile's upstream pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    CurseForge,
}

/// Kind of server controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControllerKind {
    Amp,
    Command,
}

/// Command controller configuration, used when the controller kind is `command`.
#[derive(Debug, Clone, Deserialize)]
pub struct CommandConfig {
    pub status: Vec<String>,
    pub stop: Vec<String>,
    pub start: Vec<String>,
    pub timeout_ms: Option<u64>,
}

/// Where the live server lives.
#[derive(Debug, Clone)]
pub struct ServerSection {
    pub root: PathBuf,
}

/// Which upstream pack the server follows.
#[derive(Debug, Clone)]
pub struct PackSection {
    pub provider: ProviderKind,
    pub project_id: u32,
    pub slug: Option<String>,
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

/// A fully resolved server profile.
#[derive(Debug, Clone)]
pub struct ServerProfile {
    pub name: String,
    pub server: ServerSection,
    pub pack: PackSection,
    pub overlay: OverlaySection,
    pub controller: ControllerSection,
}

/// Raw TOML shape of a profile file, before validation and path resolution.
#[derive(Debug, Deserialize)]
struct RawProfile {
    name: Option<String>,
    server: RawServer,
    pack: RawPack,
    overlay: RawOverlay,
    controller: RawController,
}

#[derive(Debug, Deserialize)]
struct RawServer {
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RawPack {
    provider: ProviderKind,
    project_id: u32,
    slug: Option<String>,
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
/// `$HOME/.config/packctl`) when the config directory exists or a home
/// directory is available; else `./packctl`.
pub fn profile_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("PACKCTL_HOME") {
        return Ok(PathBuf::from(dir));
    }

    let xdg_config = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = env::var_os("HOME").map(PathBuf::from);

    let candidate = xdg_config.map(|base| base.join("packctl")).or_else(|| {
        home.as_ref()
            .map(|home| home.join(".config").join("packctl"))
    });

    match candidate {
        Some(dir) if dir.exists() || home.is_some() => Ok(dir),
        _ => Ok(PathBuf::from("./packctl")),
    }
}

/// Load and fully resolve the profile with the given name.
pub fn load_profile(name: &str) -> Result<ServerProfile> {
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

impl RawProfile {
    fn into_profile(self, stem: &str, config_dir: &Path) -> Result<ServerProfile> {
        let name = match self.name {
            Some(name) if !name.trim().is_empty() => name,
            _ => stem.to_string(),
        };

        let server = ServerSection {
            root: resolve_against_config(config_dir, &self.server.root)?,
        };
        let pack = PackSection {
            provider: self.pack.provider,
            project_id: self.pack.project_id,
            slug: self.pack.slug,
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
        })
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
    use std::ffi::OsStr;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &OsStr) -> Self {
            let previous = env::var_os(key);
            unsafe {
                env::set_var(key, value);
            }
            EnvGuard { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    env::set_var(self.key, value);
                },
                None => unsafe {
                    env::remove_var(self.key);
                },
            }
        }
    }

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
}
