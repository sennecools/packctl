//! `packctl apikey` — set, show, or remove the encrypted CurseForge API key.
//!
//! - `--set <KEY>`, `--stdin`, or the interactive prompt store a key.
//! - `--show` reports where the key in use comes from and its last four
//!   characters, never the key itself.
//! - `--remove` deletes the stored key.
//!
//! Without `--global` the key is stored encrypted in the profile file with the
//! per-user master key (see `config::secrets`), so it never sits in plaintext
//! on disk. With `--global` it is stored once in the packctl config directory
//! and shared by every profile on the machine, so a single API key covers all
//! servers.
//!
//! A key is optional: without one, packctl reads CurseForge through the
//! community proxy (see `providers::curseforge::endpoint`). A key is sent
//! only to the official API, or to a custom endpoint allowed with
//! `PACKCTL_CF_PROXY_SEND_KEY=1`.

use std::io::BufRead;

use crate::cli::prompt::{read_secret, require_tty};
use crate::config::profile::{KeySource, ProviderKind};
use crate::error::{PackError, Result};

/// Command-line switches for `packctl apikey`.
#[derive(Debug, Clone, Default)]
pub struct ApikeyArgs {
    /// The key to store (`--set`).
    pub set: Option<String>,
    /// Read the key to store from one line of stdin (`--stdin`).
    pub stdin: bool,
    /// Report the key source and a mask (`--show`).
    pub show: bool,
    /// Remove the stored key (`--remove`).
    pub remove: bool,
    /// Manage the shared, machine-wide key (`--global`).
    pub global: bool,
}

/// Where the key to store comes from.
enum KeyInput {
    Flag(String),
    Stdin,
    Prompt,
}

impl ApikeyArgs {
    fn key_input(&self) -> KeyInput {
        match (&self.set, self.stdin) {
            (Some(key), _) => KeyInput::Flag(key.clone()),
            (None, true) => KeyInput::Stdin,
            (None, false) => KeyInput::Prompt,
        }
    }
}

/// Stores, shows, or removes the API key for a server profile, or the shared
/// machine-wide key when `--global` is set.
pub async fn run(server: Option<&str>, args: ApikeyArgs) -> Result<()> {
    if args.global {
        return run_global(server, &args);
    }

    let profile = crate::config::profile::resolve_profile(server)?;
    if args.show {
        if profile.pack.provider != ProviderKind::CurseForge {
            outln!("{}", no_key_needed(&profile.name, profile.pack.provider));
            return Ok(());
        }
        let (key, source) = profile.curseforge_api_key_with_source()?;
        let endpoint = profile.curseforge_endpoint()?.describe();
        out!("{}", show_report(source, key.as_deref(), &endpoint));
        return Ok(());
    }

    // Only CurseForge uses a key. A leftover key can still be removed.
    let leftover_removal = args.remove && profile.secrets.api_key.is_some();
    if profile.pack.provider != ProviderKind::CurseForge && !leftover_removal {
        outln!("{}", no_key_needed(&profile.name, profile.pack.provider));
        return Ok(());
    }
    let path = crate::config::profile::profile_file_path(server)?;

    if args.remove {
        crate::config::profile::update_secret_in_file(&path, None)?;
        outln!(
            "Removed the stored API key for '{}' ({}).",
            profile.name,
            path.display()
        );
        if no_key_left(false) {
            print_proxy_fallback();
        }
        return Ok(());
    }

    let key = resolve_key(args.key_input())?;
    let blob = crate::config::secrets::encrypt_string(&key)?;
    crate::config::profile::update_secret_in_file(&path, Some(&blob))?;
    outln!(
        "Stored the API key for '{}' (encrypted, {}).",
        profile.name,
        path.display()
    );
    outln!("Next: packctl status {}", profile.name);
    Ok(())
}

/// Stores, shows, or removes the shared, machine-wide API key.
fn run_global(server: Option<&str>, args: &ApikeyArgs) -> Result<()> {
    if args.show {
        let key = crate::config::secrets::load_global_key()?;
        let path = crate::config::secrets::global_key_file_path()?;
        outln!(
            "{}",
            global_show_line(key.as_deref(), &path.display().to_string())
        );
        return Ok(());
    }

    if args.remove {
        crate::config::secrets::remove_global_key()?;
        outln!("Removed the shared API key (used by every profile).");
        let profile_has_key = crate::config::profile::resolve_profile(server)
            .is_ok_and(|profile| profile.secrets.api_key.is_some());
        if !profile_has_key && no_key_left(true) {
            print_proxy_fallback();
        }
        return Ok(());
    }

    let key = resolve_key(args.key_input())?;
    crate::config::secrets::store_global_key(&key)?;
    let path = crate::config::secrets::global_key_file_path()?;
    outln!("Stored the shared API key (encrypted, {}).", path.display());
    outln!("It is now used by every profile unless a profile stores its own key.");
    Ok(())
}

/// Reads the key to store from `--set`, one line of stdin, or a hidden
/// prompt, and returns it trimmed.
fn resolve_key(input: KeyInput) -> Result<String> {
    let key = match input {
        KeyInput::Flag(key) => key,
        KeyInput::Stdin => read_stdin_line(&mut std::io::stdin().lock())?,
        KeyInput::Prompt => {
            require_tty(
                "pass --stdin (for example: printf '%s' \"$KEY\" | packctl apikey --stdin) or \
                 --set <key> to store the key non-interactively",
            )?;
            read_secret("CurseForge API key", false)?
        }
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(PackError::Config("API key must not be empty".to_string()));
    }
    Ok(key.to_string())
}

/// Reads the first line of `reader`, without its line ending.
fn read_stdin_line(reader: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|err| PackError::io("read the API key from stdin", err))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// The last four characters of `key` after an ellipsis. A key too short to
/// hide anything is not shown at all.
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.trim().chars().collect();
    if chars.len() < 8 {
        return "(too short to show)".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("…{tail}")
}

/// The `--show` report for a profile: the key source and mask, then the
/// endpoint packctl talks to.
fn show_report(source: KeySource, key: Option<&str>, endpoint: &str) -> String {
    let key_line = match key {
        Some(key) => format!(
            "CurseForge API key: {}, ends in {}",
            source.describe(),
            mask_key(key)
        ),
        None => "CurseForge API key: none".to_string(),
    };
    format!("{key_line}\nEndpoint: {endpoint}\n")
}

/// The `--show --global` line: only the shared key.
fn global_show_line(key: Option<&str>, path: &str) -> String {
    match key {
        Some(key) => format!(
            "Shared CurseForge API key: stored (encrypted, {path}), ends in {}",
            mask_key(key)
        ),
        None => "Shared CurseForge API key: none".to_string(),
    }
}

/// True when, after a removal, neither `$CF_API_KEY` nor the shared global
/// key is set. `global_removed` skips reading the global key file, which was
/// just removed. The caller checks the profile's own key.
fn no_key_left(global_removed: bool) -> bool {
    let env_key = std::env::var("CF_API_KEY").is_ok_and(|key| !key.trim().is_empty());
    let global_key =
        !global_removed && !matches!(crate::config::secrets::load_global_key(), Ok(None));
    !env_key && !global_key
}

fn print_proxy_fallback() {
    outln!(
        "No API key left; packctl will use the community proxy {}.",
        crate::providers::curseforge::endpoint::DEFAULT_PROXY_BASE_URL
    );
}

/// The message for a profile whose provider needs no key.
fn no_key_needed(name: &str, provider: ProviderKind) -> String {
    let provider = match provider {
        ProviderKind::CurseForge => "CurseForge",
        ProviderKind::Modrinth => "Modrinth",
        ProviderKind::Local => "local archives",
    };
    format!("'{name}' uses {provider}, which needs no API key.")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;
    use crate::config::profile::{
        ControllerKind, OptionalFiles, ProfileDraft, ServerProfile, load_profile, write_profile,
    };
    use crate::providers::curseforge::endpoint::{ENV_API_BASE, ENV_PROXY_SEND_KEY};
    use crate::testutil::{EnvGuard, env_lock};

    #[test]
    fn no_key_needed_names_the_provider() {
        assert_eq!(
            no_key_needed("cobble", ProviderKind::Modrinth),
            "'cobble' uses Modrinth, which needs no API key."
        );
        assert_eq!(
            no_key_needed("sb4", ProviderKind::Local),
            "'sb4' uses local archives, which needs no API key."
        );
    }

    #[test]
    fn mask_shows_last_four_only() {
        let key = "$2a$10$abcdefghijklmnopqrstuva1b2";
        let masked = mask_key(key);
        assert_eq!(masked, "…a1b2");
        assert!(!masked.contains("abcdef"));
        // Surrounding whitespace is not part of the key.
        assert_eq!(mask_key("  0123456789wxyz \n"), "…wxyz");
        // A key this short would be mostly revealed by its last four.
        assert_eq!(mask_key("abc1234"), "(too short to show)");
        // Characters, not bytes.
        assert_eq!(mask_key("ééééééééé1"), "…ééé1");
    }

    #[test]
    fn show_report_names_source_mask_and_endpoint() {
        let report = show_report(
            KeySource::Profile,
            Some("0123456789a1b2"),
            "official CurseForge API (api.curseforge.com) with your API key",
        );
        assert_eq!(
            report,
            "CurseForge API key: stored in profile (encrypted), ends in …a1b2\n\
             Endpoint: official CurseForge API (api.curseforge.com) with your API key\n"
        );
        assert!(!report.contains("0123456789"));

        let none = show_report(
            KeySource::None,
            None,
            "community proxy https://api.curse.tools (no API key configured)",
        );
        assert_eq!(
            none,
            "CurseForge API key: none\n\
             Endpoint: community proxy https://api.curse.tools (no API key configured)\n"
        );
    }

    #[test]
    fn global_show_reports_only_the_shared_key() {
        assert_eq!(
            global_show_line(Some("0123456789wxyz"), "/cfg/apikey"),
            "Shared CurseForge API key: stored (encrypted, /cfg/apikey), ends in …wxyz"
        );
        assert_eq!(
            global_show_line(None, "/cfg/apikey"),
            "Shared CurseForge API key: none"
        );
    }

    #[test]
    fn stdin_key_is_one_line_without_line_ending() {
        let mut input = std::io::Cursor::new(b"key-from-pipe\r\nsecond line\n".to_vec());
        assert_eq!(read_stdin_line(&mut input).unwrap(), "key-from-pipe");
        let mut empty = std::io::Cursor::new(Vec::new());
        assert_eq!(read_stdin_line(&mut empty).unwrap(), "");
    }

    #[test]
    fn resolve_key_trims_and_rejects_empty() {
        assert_eq!(
            resolve_key(KeyInput::Flag("  abc  ".to_string())).unwrap(),
            "abc"
        );
        assert!(matches!(
            resolve_key(KeyInput::Flag("   ".to_string())),
            Err(PackError::Config(_))
        ));
    }

    /// Writes and loads a CurseForge profile without a stored key in
    /// `$PACKCTL_HOME`.
    fn keyless_profile() -> ServerProfile {
        let draft = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv/atm10"),
            provider: ProviderKind::CurseForge,
            project_id: Some("925200".to_string()),
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
        load_profile("atm10").unwrap()
    }

    #[test]
    fn key_source_reported() {
        let _lock = env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("PACKCTL_HOME", home.path().as_os_str());
        let _env_key = EnvGuard::set("CF_API_KEY", OsStr::new(""));
        let _base = EnvGuard::set(ENV_API_BASE, OsStr::new(""));
        let _send = EnvGuard::set(ENV_PROXY_SEND_KEY, OsStr::new(""));
        let profile = keyless_profile();

        // No key anywhere: the proxy is used.
        let (key, source) = profile.curseforge_api_key_with_source().unwrap();
        assert_eq!((key, source), (None, KeySource::None));
        let endpoint = profile.curseforge_endpoint().unwrap().describe();
        assert_eq!(
            show_report(source, None, &endpoint),
            "CurseForge API key: none\n\
             Endpoint: community proxy https://api.curse.tools (no API key configured)\n"
        );

        // The environment variable.
        let _env_key = EnvGuard::set("CF_API_KEY", OsStr::new("env-key-0123-wxyz"));
        let (key, source) = profile.curseforge_api_key_with_source().unwrap();
        assert_eq!(key.as_deref(), Some("env-key-0123-wxyz"));
        assert_eq!(source, KeySource::Env);
        let endpoint = profile.curseforge_endpoint().unwrap().describe();
        assert_eq!(
            show_report(source, key.as_deref(), &endpoint),
            "CurseForge API key: from CF_API_KEY environment variable, ends in …wxyz\n\
             Endpoint: official CurseForge API (api.curseforge.com) with your API key\n"
        );
        assert_eq!(KeySource::Global.describe(), "shared global key");
    }

    // Storing a key encrypts it with a master key read from /dev/urandom.
    #[cfg(unix)]
    #[test]
    fn stored_key_sources_reported() {
        let _lock = env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("PACKCTL_HOME", home.path().as_os_str());
        let _env_key = EnvGuard::set("CF_API_KEY", OsStr::new(""));
        let _base = EnvGuard::set(ENV_API_BASE, OsStr::new(""));
        let _send = EnvGuard::set(ENV_PROXY_SEND_KEY, OsStr::new(""));
        let profile = keyless_profile();

        // The shared key.
        crate::config::secrets::store_global_key("global-key-1234").unwrap();
        let (key, source) = profile.curseforge_api_key_with_source().unwrap();
        assert_eq!(key.as_deref(), Some("global-key-1234"));
        assert_eq!(source, KeySource::Global);

        // The profile's own key wins over the shared key.
        let with_own = ServerProfile {
            secrets: crate::config::profile::SecretsSection {
                api_key: Some(crate::config::secrets::encrypt_string("profile-key-a1b2").unwrap()),
            },
            ..profile
        };
        let (key, source) = with_own.curseforge_api_key_with_source().unwrap();
        assert_eq!(key.as_deref(), Some("profile-key-a1b2"));
        assert_eq!(source, KeySource::Profile);
        let endpoint = with_own.curseforge_endpoint().unwrap().describe();
        assert_eq!(
            show_report(source, key.as_deref(), &endpoint),
            "CurseForge API key: stored in profile (encrypted), ends in …a1b2\n\
             Endpoint: official CurseForge API (api.curseforge.com) with your API key\n"
        );

        // The environment wins over everything.
        let _env_key = EnvGuard::set("CF_API_KEY", OsStr::new("env-key-wxyz"));
        let (key, source) = with_own.curseforge_api_key_with_source().unwrap();
        assert_eq!(key.as_deref(), Some("env-key-wxyz"));
        assert_eq!(source, KeySource::Env);
        assert_eq!(source.describe(), "from CF_API_KEY environment variable");
    }
}
