//! `packctl status` — report the current state of a server without any network
//! or mutation.

use crate::config::profile::ServerProfile;
use crate::controllers::ServerStatus;
use crate::core::snapshot::list_snapshots;
use crate::core::state::{InstalledState, StateStore};
use crate::error::{PackError, Result};

/// Prints the current installed state of a server profile.
///
/// Only reads local state and the controller's reported status; it never
/// contacts the pack provider or mutates anything.
pub async fn run(server: Option<&str>, json: bool) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    // Only the controller is built: status never needs the pack provider, so
    // it never decrypts an API key.
    let controller = crate::controllers::from_profile(&profile.controller)?;

    let state = StateStore::at(&profile.server.root)?.load()?;
    let snapshot_count = list_snapshots(&profile.server.root)?.len();

    // The controller may be unreachable; that is not a failure of `status`.
    let server_status = match controller.status().await {
        Ok(status) => server_status_label(status),
        Err(_) => "unknown",
    };

    if json {
        let value = status_json(&profile, &state, snapshot_count, server_status);
        let text = serde_json::to_string_pretty(&value)
            .map_err(|err| PackError::Parse(format!("failed to serialize status: {err}")))?;
        outln!("{text}");
        return Ok(());
    }

    out!(
        "{}",
        status_report(&profile.name, &state, snapshot_count, server_status)
    );
    Ok(())
}

/// The controller's status as shown by `status`: `Running`, `Stopped`, or
/// `unknown`.
fn server_status_label(status: ServerStatus) -> &'static str {
    match status {
        ServerStatus::Running => "Running",
        ServerStatus::Stopped => "Stopped",
        ServerStatus::Unknown => "unknown",
    }
}

/// The `status --json` object.
fn status_json(
    profile: &ServerProfile,
    state: &InstalledState,
    snapshot_count: usize,
    server_status: &str,
) -> serde_json::Value {
    let project = profile
        .pack
        .project_id
        .as_deref()
        .or(profile.pack.slug.as_deref());
    serde_json::json!({
        "name": &profile.name,
        "provider": profile.pack.provider.as_str(),
        "project": project,
        "installed_version": &state.installed_version,
        "provider_version_id": &state.provider_version_id,
        "last_successful_update": &state.last_successful_update,
        "managed_files": state.managed_files.len(),
        "snapshots": snapshot_count,
        "server": server_status,
        "adopted": &state.adopted,
    })
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
    if let Some(provider) = &state.provider {
        match &state.project {
            Some(project) => out.push_str(&format!("  Provider: {provider} {project}\n")),
            None => out.push_str(&format!("  Provider: {provider}\n")),
        }
    }
    if let Some(adopted) = &state.adopted {
        out.push_str(&format!(
            "  Adopted: {} on {}\n",
            adopted.version,
            adopted
                .at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ));
    }

    let last_update = state
        .last_successful_update
        .as_ref()
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
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
            .map(|index| (format!("mods/f{index}.jar"), ManagedFile::new("abc", 1)))
            .collect::<HashMap<_, _>>();
        let state = InstalledState {
            installed_version: Some("4.11".to_string()),
            provider_version_id: Some("12345".to_string()),
            managed_files: managed,
            last_successful_update: Some(timestamp("2026-08-14T12:00:00Z")),
            ..InstalledState::default()
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
    fn last_update_has_second_precision() {
        let precise = DateTime::parse_from_rfc3339("2026-08-14T12:00:05.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        let state = InstalledState {
            installed_version: Some("4.11".to_string()),
            last_successful_update: Some(precise),
            ..InstalledState::default()
        };

        let report = status_report("ATM10", &state, 0, "Running");

        assert!(
            report.contains("  Last update: 2026-08-14T12:00:05Z\n"),
            "{report}"
        );
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

    #[test]
    fn status_report_shows_provider_and_adoption() {
        let state = InstalledState {
            installed_version: Some("8.2".to_string()),
            provider_version_id: Some("YQpPe5Iu".to_string()),
            provider: Some("modrinth".to_string()),
            project: Some("5FFgwNNP".to_string()),
            adopted: Some(crate::core::state::AdoptionRecord {
                at: timestamp("2026-11-18T10:15:30Z"),
                version: "8.2".to_string(),
                provider_version_id: "YQpPe5Iu".to_string(),
                overlay_added: Vec::new(),
                ignore_rules: Vec::new(),
            }),
            ..InstalledState::default()
        };

        let report = status_report("ATM10", &state, 1, "Stopped");

        assert!(
            report.contains("  Provider: modrinth 5FFgwNNP\n"),
            "{report}"
        );
        assert!(
            report.contains("  Adopted: 8.2 on 2026-11-18T10:15:30Z\n"),
            "{report}"
        );
        let plain = status_report("ATM10", &InstalledState::default(), 0, "unknown");
        assert!(!plain.contains("Provider:") && !plain.contains("Adopted:"));
    }
    #[test]
    fn json_shape_contains_expected_keys() {
        let profile = crate::testutil::test_profile(
            std::path::Path::new("/srv/mc"),
            std::path::Path::new("/srv/mc/overlay"),
        );
        let state = InstalledState {
            installed_version: Some("8.2".to_string()),
            provider_version_id: Some("5002".to_string()),
            managed_files: (0..3)
                .map(|index| (format!("mods/f{index}.jar"), ManagedFile::new("abc", 1)))
                .collect(),
            last_successful_update: Some(timestamp("2026-08-14T12:00:00Z")),
            ..InstalledState::default()
        };

        let value = status_json(
            &profile,
            &state,
            2,
            server_status_label(ServerStatus::Stopped),
        );

        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "adopted",
                "installed_version",
                "last_successful_update",
                "managed_files",
                "name",
                "project",
                "provider",
                "provider_version_id",
                "server",
                "snapshots",
            ]
        );
        assert_eq!(value["name"], "test-server");
        assert_eq!(value["provider"], "curseforge");
        assert_eq!(value["project"], "42");
        assert_eq!(value["installed_version"], "8.2");
        assert_eq!(value["provider_version_id"], "5002");
        assert_eq!(value["last_successful_update"], "2026-08-14T12:00:00Z");
        assert_eq!(value["managed_files"], 3);
        assert_eq!(value["snapshots"], 2);
        assert_eq!(value["server"], "Stopped");
        assert!(value["adopted"].is_null());

        // A never-updated server reports nulls, not missing keys.
        let empty = status_json(&profile, &InstalledState::default(), 0, "unknown");
        assert!(empty["installed_version"].is_null());
        assert!(empty["last_successful_update"].is_null());
        assert_eq!(empty["server"], "unknown");
        assert_eq!(server_status_label(ServerStatus::Running), "Running");
        assert_eq!(server_status_label(ServerStatus::Unknown), "unknown");
    }
}
