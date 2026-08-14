//! Rollback command.
//!
//! Restores the most recent snapshot so the server returns to its previous
//! successful version. The server is stopped before managed files are mutated
//! and restarted afterwards (see design notes "Rollback").

use crate::core::snapshot::{list_snapshots, restore_snapshot};
use crate::core::updater::Updater;
use crate::error::{PackError, Result};

/// Restores the latest snapshot of a server profile.
pub async fn run(server: Option<&str>) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let snapshots = list_snapshots(&profile.server.root)?;
    let latest = match snapshots.first() {
        Some(snapshot) => snapshot,
        None => {
            return Err(PackError::NotFound(
                "no snapshots to roll back to".to_string(),
            ));
        }
    };

    updater.controller.stop().await?;
    restore_snapshot(&profile.server.root, latest)?;
    updater.controller.start().await?;

    let state = updater.load_state()?;
    let installed_version = state.installed_version.as_deref().unwrap_or("unknown");
    let provider_version_id = state.provider_version_id.as_deref().unwrap_or("unknown");

    println!("Rolled back to {installed_version} (id {provider_version_id})");
    println!("Snapshot: {}", latest.dir.display());
    Ok(())
}
