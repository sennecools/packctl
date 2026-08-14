//! `packctl update` — apply an update to a server.

use std::io::IsTerminal;

use crate::core::updater::{UpdateOutcome, Updater};
use crate::error::{PackError, Result};
use crate::providers::VersionSelector;

/// Prepares, previews, confirms, and applies an update for a server profile.
pub async fn run(
    server: Option<&str>,
    version: Option<&str>,
    non_interactive: bool,
    verbose: bool,
) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let selector = select_version(&updater, version, non_interactive).await?;

    let prepared = updater.prepare_update(&selector).await?;
    crate::cli::plan::print_plan(&profile, &prepared.plan, verbose)?;

    if prepared.plan.is_empty() {
        println!("Already up to date ({}).", prepared.plan.to_version);
        return Ok(());
    }

    if !non_interactive {
        if !std::io::stdin().is_terminal() {
            return Err(PackError::Config(
                "stdin is not a terminal; pass --non-interactive to apply without confirmation"
                    .to_string(),
            ));
        }
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Apply update?")
            .interact()
            .map_err(|err| dialoguer_error("read update confirmation", err))?;
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    let outcome = updater.execute(&prepared).await?;
    print!("{}", success_summary(&outcome));
    Ok(())
}

/// Resolves the target [`VersionSelector`] from the requested version and the
/// terminal state.
///
/// An explicit version always wins. Without one, non-interactive runs use the
/// latest version; interactive runs offer a numbered menu with a leading
/// "(latest)" convenience option.
async fn select_version(
    updater: &Updater,
    version: Option<&str>,
    non_interactive: bool,
) -> Result<VersionSelector> {
    if let Some(version) = version {
        return Ok(VersionSelector::Name(version.to_string()));
    }

    if non_interactive || !std::io::stdin().is_terminal() {
        return Ok(VersionSelector::Latest);
    }

    let versions = updater.provider.list_versions(&updater.pack_ref()).await?;
    if versions.is_empty() {
        return Err(PackError::Provider(format!(
            "no versions available for server '{}'",
            updater.profile.name
        )));
    }

    let mut items: Vec<String> = Vec::with_capacity(versions.len() + 1);
    items.push("(latest)".to_string());
    items.extend(versions.iter().map(|version| version.name.clone()));

    let chosen = dialoguer::Select::new()
        .with_prompt("Select a version")
        .items(&items)
        .interact()
        .map_err(|err| dialoguer_error("select a version", err))?;

    if chosen == 0 {
        Ok(VersionSelector::Latest)
    } else {
        Ok(VersionSelector::Name(versions[chosen - 1].name.clone()))
    }
}

/// Wraps a [`dialoguer::Error`] with the operation that failed.
fn dialoguer_error(what: &str, err: dialoguer::Error) -> PackError {
    match err {
        dialoguer::Error::IO(source) => PackError::io(what, source),
    }
}

/// Renders the success report after an update was applied.
fn success_summary(outcome: &UpdateOutcome) -> String {
    let mut lines = if outcome.committed {
        vec!["Update applied and committed.".to_string()]
    } else {
        vec!["Update applied (state not committed).".to_string()]
    };
    lines.push(format!(
        "  upstream: {} files written",
        outcome.upstream_writes
    ));
    lines.push(format!(
        "  overlay: {} files copied",
        outcome.overlay_copied
    ));
    if let Some(snapshot) = &outcome.snapshot {
        lines.push("Rollback snapshot:".to_string());
        lines.push(format!("  {}", snapshot.dir.display()));
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use chrono::Utc;

    use super::*;
    use crate::core::snapshot::{Snapshot, SnapshotManifest};

    fn snapshot() -> Snapshot {
        let created = Utc::now();
        Snapshot {
            dir: PathBuf::from("/srv/.packctl/snapshots/2026-08-14T22-41-10Z"),
            created,
            manifest: SnapshotManifest {
                created,
                files: HashMap::new(),
                tracked_paths: Vec::new(),
            },
        }
    }

    #[test]
    fn success_summary_reports_counts_and_snapshot() {
        let outcome = UpdateOutcome {
            upstream_writes: 2,
            overlay_copied: 1,
            snapshot: Some(snapshot()),
            committed: true,
        };

        let summary = success_summary(&outcome);

        assert!(summary.contains("Update applied and committed."));
        assert!(summary.contains("  upstream: 2 files written"));
        assert!(summary.contains("  overlay: 1 files copied"));
        assert!(summary.contains("Rollback snapshot:"));
        assert!(
            summary.contains("/srv/.packctl/snapshots/2026-08-14T22-41-10Z"),
            "summary: {summary}"
        );
    }

    #[test]
    fn success_summary_omits_snapshot_when_absent() {
        let outcome = UpdateOutcome {
            upstream_writes: 0,
            overlay_copied: 0,
            snapshot: None,
            committed: false,
        };

        let summary = success_summary(&outcome);

        assert!(summary.contains("Update applied (state not committed)."));
        assert!(!summary.contains("Rollback snapshot:"));
    }
}
