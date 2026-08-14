//! `packctl status` — report the current state of a server without any network
//! or mutation.

use crate::core::snapshot::list_snapshots;
use crate::core::state::InstalledState;
use crate::core::updater::Updater;
use crate::error::Result;

/// Prints the current installed state of a server profile.
///
/// Only reads local state and the controller's reported status; it never
/// contacts the pack provider or mutates anything.
pub async fn run(server: Option<&str>) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let state = updater.load_state()?;
    let snapshot_count = list_snapshots(&profile.server.root)?.len();

    // The controller may be unreachable; that is not a failure of `status`.
    let server_status = match updater.controller.status().await {
        Ok(status) => format!("{status:?}"),
        Err(_) => "unknown".to_string(),
    };

    print!(
        "{}",
        status_report(&profile.name, &state, snapshot_count, &server_status)
    );
    Ok(())
}

/// Renders the status report block.
fn status_report(
    profile_name: &str,
    state: &InstalledState,
    snapshot_count: usize,
    server_status: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("{profile_name}\n"));

    let installed = state.installed_version.as_deref().unwrap_or("never");
    match &state.provider_version_id {
        Some(id) => out.push_str(&format!("  Installed: {installed} (id {id})\n")),
        None => out.push_str(&format!("  Installed: {installed}\n")),
    }

    let last_update = state
        .last_successful_update
        .as_ref()
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        .unwrap_or_else(|| "never".to_string());
    out.push_str(&format!("  Last update: {last_update}\n"));
    out.push_str(&format!("  Managed files: {}\n", state.managed_files.len()));
    out.push_str(&format!("  Snapshots: {snapshot_count}\n"));
    out.push_str(&format!("  Server: {server_status}\n"));

    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{DateTime, Utc};

    use super::*;
    use crate::core::state::ManagedFile;

    fn timestamp(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn status_report_renders_installed_state() {
        let managed = (0..247)
            .map(|index| {
                (
                    format!("mods/f{index}.jar"),
                    ManagedFile {
                        sha256: "abc".to_string(),
                        size: 1,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let state = InstalledState {
            installed_version: Some("4.11".to_string()),
            provider_version_id: Some("12345".to_string()),
            managed_files: managed,
            last_successful_update: Some(timestamp("2026-08-14T12:00:00Z")),
        };

        let report = status_report("ATM10", &state, 3, "Running");

        assert!(report.contains("ATM10"));
        assert!(report.contains("  Installed: 4.11 (id 12345)"));
        assert!(report.contains("  Last update: 2026-08-14T12:00:00Z"));
        assert!(report.contains("  Managed files: 247"));
        assert!(report.contains("  Snapshots: 3"));
        assert!(report.contains("  Server: Running"));
    }

    #[test]
    fn status_report_handles_never_installed() {
        let report = status_report("ATM10", &InstalledState::default(), 0, "unknown");

        assert!(report.contains("  Installed: never"));
        assert!(report.contains("  Last update: never"));
        assert!(report.contains("  Managed files: 0"));
        assert!(report.contains("  Snapshots: 0"));
        assert!(report.contains("  Server: unknown"));
    }

    #[test]
    fn status_report_omits_id_when_provider_id_absent() {
        let state = InstalledState {
            installed_version: Some("4.11".to_string()),
            ..InstalledState::default()
        };

        let report = status_report("ATM10", &state, 0, "Stopped");

        assert!(report.contains("  Installed: 4.11"));
        assert!(!report.contains("(id"));
        assert!(report.contains("  Server: Stopped"));
    }
}
