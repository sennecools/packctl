//! Validate command.
//!
//! Checks that the installed server matches its recorded state and runs the
//! environment checks. The command exits non-zero when anything fails (see
//! design notes "Validation" and "State Store").

use std::path::Path;

use crate::core::overlay::OverlayEngine;
use crate::core::state::InstalledState;
use crate::core::updater::Updater;
use crate::core::validation::{Severity, ValidationIssue, has_errors, validate};
use crate::error::{PackError, Result};
use crate::fs::hashing::sha256_file;
use crate::fs::paths::safe_join;

/// Validates a server against its recorded state plus the environment checks.
pub async fn run(server: Option<&str>) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let state = updater.load_state()?;
    let mut issues = check_managed_files(&profile.server.root, &profile.name, &state)?;

    let overlay_files = OverlayEngine::new(profile.overlay.path.clone()).scan()?;
    let env_issues = validate(&profile, None, &overlay_files, updater.controller.as_ref()).await?;
    issues.extend(env_issues);

    print_issues(&issues);
    println!("{}", issue_summary(&issues));

    if has_errors(&issues) {
        return Err(PackError::Validation("validation failed".into()));
    }
    Ok(())
}

/// Compares every recorded managed file against the server on disk.
///
/// A file that no longer exists or whose content no longer matches its
/// recorded fingerprint is an error issue. `profile_name` adds context when a
/// managed path is rejected as unsafe.
fn check_managed_files(
    server_root: &Path,
    profile_name: &str,
    state: &InstalledState,
) -> Result<Vec<ValidationIssue>> {
    let mut issues = Vec::new();
    for (rel, managed) in &state.managed_files {
        let path = safe_join(server_root, Path::new(rel))
            .map_err(|error| PackError::Other(format!("profile '{profile_name}': {error}")))?;
        if !path.exists() {
            issues.push(ValidationIssue {
                severity: Severity::Error,
                message: format!("managed file is missing: {rel}"),
            });
        } else if sha256_file(&path)? != managed.sha256 {
            issues.push(ValidationIssue {
                severity: Severity::Error,
                message: format!("managed file content differs from recorded state: {rel}"),
            });
        }
    }
    Ok(issues)
}

/// Prints each finding as `[warning] ...` or `[error] ...`.
fn print_issues(issues: &[ValidationIssue]) {
    for issue in issues {
        match issue.severity {
            Severity::Warning => println!("[warning] {}", issue.message),
            Severity::Error => println!("[error] {}", issue.message),
        }
    }
}

/// Builds the `N warning(s), M error(s)` summary line.
fn issue_summary(issues: &[ValidationIssue]) -> String {
    let warnings = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Warning)
        .count();
    let errors = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .count();
    format!("{warnings} warning(s), {errors} error(s)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::state::{ManagedFile, StateStore};
    use crate::fs::hashing::sha256_bytes;

    const PROFILE: &str = "test-server";

    /// State recording one matching, one missing, and one tampered file.
    fn state_with_managed_files() -> InstalledState {
        let mut managed_files = std::collections::HashMap::new();
        managed_files.insert(
            "mods/matching.jar".to_string(),
            ManagedFile {
                sha256: sha256_bytes(b"matching content"),
                size: 16,
            },
        );
        managed_files.insert(
            "mods/missing.jar".to_string(),
            ManagedFile {
                sha256: sha256_bytes(b"never on disk"),
                size: 13,
            },
        );
        managed_files.insert(
            "mods/tampered.jar".to_string(),
            ManagedFile {
                sha256: sha256_bytes(b"original content"),
                size: 16,
            },
        );
        InstalledState {
            installed_version: Some("4.11".to_string()),
            provider_version_id: Some("12345".to_string()),
            managed_files,
            ..InstalledState::default()
        }
    }

    /// Server root where `matching.jar` matches state and `tampered.jar` does
    /// not; `missing.jar` is absent.
    fn server_root_with_disk_files(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        let root = tmp.path().join("server");
        std::fs::create_dir_all(root.join("mods")).unwrap();
        std::fs::write(root.join("mods/matching.jar"), b"matching content").unwrap();
        std::fs::write(root.join("mods/tampered.jar"), b"tampered content!").unwrap();
        root
    }

    #[test]
    fn flags_missing_and_tampered_but_not_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = server_root_with_disk_files(&tmp);
        let state = state_with_managed_files();

        let issues = check_managed_files(&root, PROFILE, &state).unwrap();

        let messages: Vec<&str> = issues.iter().map(|issue| issue.message.as_str()).collect();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("managed file is missing: mods/missing.jar")),
            "missing file not flagged: {messages:?}"
        );
        assert!(
            messages.iter().any(|message| message
                .contains("managed file content differs from recorded state: mods/tampered.jar")),
            "tampered file not flagged: {messages:?}"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("matching.jar")),
            "matching file should not be flagged: {messages:?}"
        );
        assert!(
            issues.iter().all(|issue| issue.severity == Severity::Error),
            "all findings must be errors: {issues:#?}"
        );
    }

    #[test]
    fn matching_files_produce_no_issues() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(root.join("mods")).unwrap();
        std::fs::write(root.join("mods/matching.jar"), b"matching content").unwrap();

        let mut state = InstalledState::default();
        state.managed_files.insert(
            "mods/matching.jar".to_string(),
            ManagedFile {
                sha256: sha256_bytes(b"matching content"),
                size: 16,
            },
        );

        let issues = check_managed_files(&root, PROFILE, &state).unwrap();
        assert!(issues.is_empty(), "issues: {issues:?}");
    }

    #[test]
    fn state_store_round_trip_feeds_managed_file_check() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(root.join("mods")).unwrap();
        std::fs::write(root.join("mods/ok.jar"), b"data").unwrap();

        let store = StateStore::at(&root).unwrap();
        let mut state = InstalledState::default();
        state.managed_files.insert(
            "mods/ok.jar".to_string(),
            ManagedFile {
                sha256: sha256_bytes(b"data"),
                size: 4,
            },
        );
        store.save(&state).unwrap();

        let loaded = store.load().unwrap();
        let issues = check_managed_files(&root, PROFILE, &loaded).unwrap();
        assert!(issues.is_empty(), "issues: {issues:?}");
    }
}
