//! `packctl update` — apply an update to a server.

use std::io::IsTerminal;

use crate::cli::prompt::{dialoguer_error, require_tty};
use crate::core::updater::{ExecuteOptions, UpdateOutcome, Updater};
use crate::error::{PackError, Result};
use crate::providers::VersionSelector;

/// Command-line switches for `packctl update`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateFlags {
    /// Never prompt; fail where confirmation would be required.
    pub non_interactive: bool,
    /// Print the full file-level plan.
    pub verbose: bool,
    /// Apply even when the plan deletes files packctl never installed or
    /// overwrites live files changed outside packctl.
    pub force_sync: bool,
}

/// Prepares, previews, confirms, and applies an update for a server profile.
pub async fn run(server: Option<&str>, version: Option<&str>, flags: UpdateFlags) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let selector = select_version(&updater, version, flags.non_interactive).await?;

    let prepared = updater.prepare_update(&selector).await?;
    out!(
        "{}",
        crate::cli::plan::render_plan(&profile, &prepared, server, flags.verbose)
    );

    if !prepared.needs_state_commit() {
        outln!("Already up to date ({}).", prepared.plan.to_version);
        return Ok(());
    }

    if prepared.plan.is_empty() {
        // The server already matches the target; only the record changes,
        // so there is nothing to confirm.
        updater
            .execute(&prepared, &execute_options(server, false))
            .await?;
        outln!("Recorded {}; no file changes.", prepared.plan.to_version);
        return Ok(());
    }

    let mut force_sync = flags.force_sync;
    if prepared.needs_force_sync() && !force_sync && !flags.non_interactive {
        require_terminal(FORCE_SYNC_FLAGS)?;
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Delete or overwrite the files listed above anyway?")
            .default(false)
            .interact()
            .map_err(|err| dialoguer_error("read force-sync confirmation", err))?;
        if !confirmed {
            outln!("Aborted.");
            return Ok(());
        }
        force_sync = true;
    }

    if !flags.non_interactive {
        // A plan that needs --force-sync has it by now (flag or prompt), so
        // --non-interactive alone is enough here.
        require_terminal(APPLY_FLAGS)?;
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Apply update?")
            .interact()
            .map_err(|err| dialoguer_error("read update confirmation", err))?;
        if !confirmed {
            outln!("Aborted.");
            return Ok(());
        }
    }

    let outcome = updater
        .execute(&prepared, &execute_options(server, force_sync))
        .await?;
    out!("{}", success_summary(&outcome));
    Ok(())
}

/// The execute options for the server the user named.
fn execute_options(server: Option<&str>, force_sync: bool) -> ExecuteOptions {
    ExecuteOptions {
        force_sync,
        adopt_command: Some(crate::cli::adopt::adopt_command(server)),
    }
}

/// Switches that apply an ordinary plan without prompting.
const APPLY_FLAGS: &str = "--non-interactive";

/// Switches that apply a plan needing `--force-sync` without prompting;
/// `execute` refuses such a plan under `--non-interactive` alone.
const FORCE_SYNC_FLAGS: &str = "--non-interactive --force-sync";

/// Confirmation prompts need a terminal.
///
/// `flags` names the switches that skip the prompt, so the hint never points
/// at a command line that `execute` would then refuse.
fn require_terminal(flags: &str) -> Result<()> {
    require_tty(&apply_hint(flags))
}

/// How to apply without a confirmation prompt.
fn apply_hint(flags: &str) -> String {
    format!("pass {flags} to apply without confirmation")
}

/// The error for a confirmation prompt without a terminal.
#[cfg(test)]
fn not_a_terminal_error(flags: &str) -> PackError {
    crate::cli::prompt::not_a_tty_error(&apply_hint(flags))
}

/// Resolves the target [`VersionSelector`] from the requested version and the
/// terminal state.
///
/// An explicit version always wins and is matched loosely (id, name, or a
/// unique part of a name). Without one, non-interactive runs use the latest
/// version; interactive runs offer a numbered menu with a leading "(latest)"
/// convenience option, resolved by id.
async fn select_version(
    updater: &Updater,
    version: Option<&str>,
    non_interactive: bool,
) -> Result<VersionSelector> {
    if let Some(version) = version {
        return Ok(VersionSelector::Query(version.to_string()));
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
        Ok(VersionSelector::Id(versions[chosen - 1].id.clone()))
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
                state_refresh: false,
                restored: None,
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

    #[test]
    fn not_a_terminal_hint_names_force_sync_when_the_plan_needs_it() {
        let ordinary = not_a_terminal_error(APPLY_FLAGS).to_string();
        assert!(
            ordinary.contains("pass --non-interactive to apply"),
            "{ordinary}"
        );
        assert!(!ordinary.contains("--force-sync"), "{ordinary}");

        let forced = not_a_terminal_error(FORCE_SYNC_FLAGS).to_string();
        assert!(
            forced.contains("pass --non-interactive --force-sync to apply"),
            "{forced}"
        );
    }
}
