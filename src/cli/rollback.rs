//! Rollback command.
//!
//! Restores the most recent snapshot, undoing the last packctl change: a file
//! update, or a commit that recorded a new version without changing files
//! (restoring that one reverts only the record). Newer state-refresh
//! snapshots, taken before a commit that kept the recorded version and only
//! rewrote `state.json` bookkeeping, are passed over. The server is stopped
//! before managed files are mutated and restarted afterwards (see design
//! notes "Rollback").

use crate::core::snapshot::{list_snapshots, mark_restored, restore_snapshot, rollback_target};
use crate::core::state::{InstalledState, StateStore};
use crate::error::{PackError, Result};

/// Restores the latest snapshot of a server profile (see [`rollback_target`]).
pub async fn run(server: Option<&str>) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    // Rollback restores local snapshots only; it never builds the pack
    // provider, so it never decrypts an API key.
    let controller = crate::controllers::from_profile(&profile.controller)?;
    let store = StateStore::at(&profile.server.root)?;
    let _lock = store.lock()?;

    let snapshots = list_snapshots(&profile.server.root)?;
    let Some((target, skipped)) = rollback_target(&snapshots) else {
        return Err(PackError::NotFound(
            "no snapshots to roll back to".to_string(),
        ));
    };

    controller.stop().await?;
    restore_snapshot(&profile.server.root, target)?;
    // Marked so the next rollback goes one step further back instead of
    // replaying this one. A failure here must not keep the server down.
    let marked = mark_restored(target);
    controller.start().await?;
    if let Err(error) = marked {
        errln!("warning: could not mark the snapshot as restored: {error}");
    }

    let state = store.load()?;
    outln!("{}", rolled_back_message(&state));
    if let Some(note) = skipped_note(skipped) {
        outln!("{note}");
    }
    outln!("Snapshot: {}", target.dir.display());
    Ok(())
}

/// Explains that newer state-refresh snapshots were passed over.
fn skipped_note(skipped: usize) -> Option<String> {
    match skipped {
        0 => None,
        1 => Some(
            "Passed over 1 newer snapshot that only refreshed the record of the same version."
                .to_string(),
        ),
        n => Some(format!(
            "Passed over {n} newer snapshots that only refreshed the record of the same version."
        )),
    }
}

/// Describes the state a rollback restored.
fn rolled_back_message(state: &InstalledState) -> String {
    match &state.installed_version {
        Some(version) => {
            let id = state.provider_version_id.as_deref().unwrap_or("unknown");
            format!("Rolled back to {version} (id {id})")
        }
        None => "Rolled back to the state before the last packctl change (no version recorded)"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolled_back_message_names_the_version_or_says_none_was_recorded() {
        let state = InstalledState {
            installed_version: Some("4.11".to_string()),
            provider_version_id: Some("11".to_string()),
            ..InstalledState::default()
        };
        assert_eq!(rolled_back_message(&state), "Rolled back to 4.11 (id 11)");
        assert_eq!(
            rolled_back_message(&InstalledState::default()),
            "Rolled back to the state before the last packctl change (no version recorded)"
        );
    }

    #[test]
    fn skipped_note_only_appears_when_snapshots_were_passed_over() {
        assert_eq!(skipped_note(0), None);
        assert_eq!(
            skipped_note(1).as_deref(),
            Some(
                "Passed over 1 newer snapshot that only refreshed the record of the same version."
            )
        );
        assert_eq!(
            skipped_note(2).as_deref(),
            Some(
                "Passed over 2 newer snapshots that only refreshed the record of the same version."
            )
        );
    }
}
