//! `packctl apikey` — manage the encrypted CurseForge API key.
//!
//! Without `--global` the key is stored encrypted in the profile file with the
//! per-user master key (see `config::secrets`), so it never sits in plaintext
//! on disk. With `--global` it is stored once in the packctl config directory
//! and shared by every profile on the machine, so a single API key covers all
//! servers.

use std::io::IsTerminal;

use crate::error::{PackError, Result};

/// Stores, shows, or removes the API key for a server profile, or the shared
/// machine-wide key when `global` is set.
pub async fn run(
    server: Option<&str>,
    set: Option<String>,
    remove: bool,
    global: bool,
) -> Result<()> {
    if global {
        return run_global(set, remove);
    }

    let profile = crate::config::profile::resolve_profile(server)?;
    let path = crate::config::profile::profile_file_path(server)?;

    if remove {
        crate::config::profile::update_secret_in_file(&path, None)?;
        println!(
            "Removed the stored API key for '{}' ({}).",
            profile.name,
            path.display()
        );
        return Ok(());
    }

    let key = resolve_key(set)?;
    let blob = crate::config::secrets::encrypt_string(key.trim())?;
    crate::config::profile::update_secret_in_file(&path, Some(&blob))?;
    println!(
        "Stored the API key for '{}' (encrypted, {}).",
        profile.name,
        path.display()
    );
    println!("Next: packctl status {}", profile.name);
    Ok(())
}

/// Stores or removes the shared, machine-wide API key.
fn run_global(set: Option<String>, remove: bool) -> Result<()> {
    if remove {
        crate::config::secrets::remove_global_key()?;
        println!("Removed the shared API key (used by every profile).");
        return Ok(());
    }

    let key = resolve_key(set)?;
    crate::config::secrets::store_global_key(key.trim())?;
    let path = crate::config::secrets::global_key_file_path()?;
    println!("Stored the shared API key (encrypted, {}).", path.display());
    println!("It is now used by every profile unless a profile stores its own key.");
    Ok(())
}

/// Reads the key to store from `--set` or an interactive prompt.
fn resolve_key(set: Option<String>) -> Result<String> {
    let key = match set {
        Some(key) => key,
        None if std::io::stdin().is_terminal() => dialoguer::Input::new()
            .with_prompt("CurseForge API key")
            .interact_text()
            .map_err(|err| dialoguer_error("read API key", err))?,
        None => {
            return Err(PackError::Config(
                "stdin is not a terminal; pass --set <key> to store the key non-interactively"
                    .to_string(),
            ));
        }
    };
    if key.trim().is_empty() {
        return Err(PackError::Config("API key must not be empty".to_string()));
    }
    Ok(key)
}

/// Wraps a [`dialoguer::Error`] with the operation that failed.
fn dialoguer_error(what: &str, err: dialoguer::Error) -> PackError {
    match err {
        dialoguer::Error::IO(source) => PackError::io(what, source),
    }
}
