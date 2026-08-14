//! `packctl apikey` — manage the encrypted CurseForge API key for a server.
//!
//! The key is stored encrypted in the profile file with the per-user master
//! key (see `config::secrets`), so it never sits in plaintext on disk.

use std::io::IsTerminal;

use crate::error::{PackError, Result};

/// Stores, shows, or removes the API key stored with a server profile.
pub async fn run(server: Option<&str>, set: Option<String>, remove: bool) -> Result<()> {
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

/// Wraps a [`dialoguer::Error`] with the operation that failed.
fn dialoguer_error(what: &str, err: dialoguer::Error) -> PackError {
    match err {
        dialoguer::Error::IO(source) => PackError::io(what, source),
    }
}
