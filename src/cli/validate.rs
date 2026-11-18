//! Validate command.
//!
//! Checks that the installed server matches its recorded state and runs the
//! environment checks. The command exits non-zero when anything fails (see
//! design notes "Validation" and "State Store").

use std::path::Path;

use crate::config::profile::{ProviderKind, ServerProfile};
use crate::core::ignore::IgnoreRules;
use crate::core::overlay::OverlayEngine;
use crate::core::state::InstalledState;
use crate::core::state::StateStore;
use crate::core::updater::load_ignore_rules;
use crate::core::validation::{Severity, ValidationIssue, has_errors, validate};
use crate::error::{PackError, Result};
use crate::fs::hashing::sha256_file;
use crate::fs::paths::safe_join;

/// Validates a server against its recorded state plus the environment checks.
pub async fn run(server: Option<&str>) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    // Built apart from the provider (see [`provider_issue`]), so a broken API
    // key does not stop the checks.
    let controller = crate::controllers::from_profile(&profile.controller)?;

    let state = StateStore::at(&profile.server.root)?.load()?;
    let ignore = load_ignore_rules(&profile.server.root)?;
    let mut issues = check_managed_files(&profile.server.root, &profile.name, &state, &ignore)?;
    issues.extend(provider_issue(&profile));

    let overlay_files = OverlayEngine::new(profile.overlay.path.clone()).scan()?;
    let env_issues = validate(&profile, None, &overlay_files, controller.as_ref()).await?;
    issues.extend(env_issues);

    print_issues(&issues);
    outln!("{}", issue_summary(&issues));

    if has_errors(&issues) {
        return Err(PackError::Validation(format!(
            "{} error(s) found",
            error_count(&issues)
        )));
    }
    Ok(())
}

/// Compares every recorded managed file against the server on disk.
///
/// A file that no longer exists or whose content no longer matches its
/// recorded fingerprint is an error issue. Paths matched by `.packctlignore`
/// are skipped: packctl no longer owns them, even when an older state still
/// lists them. `profile_name` adds context when a managed path is rejected as
/// unsafe.
fn check_managed_files(
    server_root: &Path,
    profile_name: &str,
    state: &InstalledState,
    ignore: &IgnoreRules,
) -> Result<Vec<ValidationIssue>> {
    let mut issues = Vec::new();
    let mut managed: Vec<_> = state.managed_files.iter().collect();
    managed.sort_by(|a, b| a.0.cmp(b.0));
    for (rel, managed) in managed {
        if ignore.is_ignored(Path::new(rel)) {
            continue;
        }
        let path = safe_join(server_root, Path::new(rel))
            .map_err(|error| PackError::Other(format!("profile '{profile_name}': {error}")))?;
        if !path.exists() {
            issues.push(ValidationIssue {
                severity: Severity::Error,
                message: format!("managed file is missing: {rel}"),
            });
        } else if !matches_record(&sha256_file(&path)?, managed) {
            issues.push(ValidationIssue {
                severity: Severity::Error,
                message: format!("managed file content differs from recorded state: {rel}"),
            });
        }
    }
    Ok(issues)
}

/// True when a live hash is the recorded one or its accepted local variant
/// (a config adopt found only reformatted).
fn matches_record(sha256: &str, managed: &crate::core::state::ManagedFile) -> bool {
    sha256 == managed.sha256 || managed.local_sha256.as_deref() == Some(sha256)
}

/// Builds the profile's pack provider and reports a failure as an error
/// finding instead of an early exit, so `doctor` and `validate` still run
/// every other check (the usual cause is an API key that no longer decrypts,
/// which is exactly what `doctor` should explain).
pub(super) fn provider_issue(profile: &ServerProfile) -> Option<ValidationIssue> {
    let error = crate::providers::from_profile(profile).err()?;
    // A CurseForge setup failure is usually the key; an invalid
    // $PACKCTL_CF_API_BASE gets the generic message.
    let key_failed =
        profile.pack.provider == ProviderKind::CurseForge && profile.curseforge_api_key().is_err();
    let message = match profile.pack.provider {
        ProviderKind::CurseForge if key_failed => format!(
            "CurseForge key could not be decrypted: {error}; run 'packctl apikey --remove' \
             and store the key again"
        ),
        provider => format!("pack provider '{provider}' could not be set up: {error}"),
    };
    Some(ValidationIssue {
        severity: Severity::Error,
        message,
    })
}

/// Prints each finding as `[warning] ...` or `[error] ...`.
fn print_issues(issues: &[ValidationIssue]) {
    for issue in issues {
        match issue.severity {
            Severity::Warning => outln!("[warning] {}", issue.message),
            Severity::Error => outln!("[error] {}", issue.message),
        }
    }
}

/// Builds the `N warning(s), M error(s)` summary line.
///
/// Shared with the doctor command so both report findings the same way.
pub(super) fn issue_summary(issues: &[ValidationIssue]) -> String {
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

/// How many findings are errors.
pub(super) fn error_count(issues: &[ValidationIssue]) -> usize {
    issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .count()
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
            ManagedFile::new(sha256_bytes(b"matching content"), 16),
        );
        managed_files.insert(
            "mods/missing.jar".to_string(),
            ManagedFile::new(sha256_bytes(b"never on disk"), 13),
        );
        managed_files.insert(
            "mods/tampered.jar".to_string(),
            ManagedFile::new(sha256_bytes(b"original content"), 16),
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

        let issues = check_managed_files(&root, PROFILE, &state, &IgnoreRules::default()).unwrap();

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
            ManagedFile::new(sha256_bytes(b"matching content"), 16),
        );

        let issues = check_managed_files(&root, PROFILE, &state, &IgnoreRules::default()).unwrap();
        assert!(issues.is_empty(), "issues: {issues:?}");
    }

    #[test]
    fn accepted_local_variant_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config/a.toml"), b"# mine\na = 1\n").unwrap();

        let mut state = InstalledState::default();
        state.managed_files.insert(
            "config/a.toml".to_string(),
            ManagedFile::new(sha256_bytes(b"a = 1\n"), 6)
                .with_local(sha256_bytes(b"# mine\na = 1\n")),
        );
        let issues = check_managed_files(&root, PROFILE, &state, &IgnoreRules::default()).unwrap();
        assert!(issues.is_empty(), "issues: {issues:?}");

        std::fs::write(root.join("config/a.toml"), b"a = 2\n").unwrap();
        let issues = check_managed_files(&root, PROFILE, &state, &IgnoreRules::default()).unwrap();
        assert_eq!(issues.len(), 1, "issues: {issues:?}");
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
            ManagedFile::new(sha256_bytes(b"data"), 4),
        );
        store.save(&state).unwrap();

        let loaded = store.load().unwrap();
        let issues = check_managed_files(&root, PROFILE, &loaded, &IgnoreRules::default()).unwrap();
        assert!(issues.is_empty(), "issues: {issues:?}");
    }

    #[test]
    fn ignored_managed_paths_are_not_reported() {
        // A state written before ignored paths were excluded from it.
        let tmp = tempfile::tempdir().unwrap();
        let root = server_root_with_disk_files(&tmp);
        let state = state_with_managed_files();
        let ignore =
            crate::core::ignore::parse_rules("mods/missing.jar\nmods/tampered.jar\n").unwrap();

        let issues = check_managed_files(&root, PROFILE, &state, &ignore).unwrap();

        assert!(issues.is_empty(), "issues: {issues:?}");
    }

    fn profile_with(pack: crate::config::profile::PackSection) -> ServerProfile {
        use crate::config::profile::{
            ControllerKind, ControllerSection, OverlaySection, SecretsSection, ServerSection,
        };
        ServerProfile {
            name: PROFILE.to_string(),
            server: ServerSection {
                root: std::path::PathBuf::from("/srv/mc"),
            },
            pack,
            overlay: OverlaySection {
                path: std::path::PathBuf::from("/srv/mc/overlay"),
            },
            controller: ControllerSection {
                kind: ControllerKind::Command,
                instance: None,
                command: None,
            },
            secrets: SecretsSection::default(),
        }
    }

    #[test]
    fn provider_issue_reports_setup_failures_as_errors() {
        use crate::config::profile::{PackSection, SecretsSection};
        use crate::testutil::{EnvGuard, env_lock};

        let _lock = env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("PACKCTL_HOME", home.path().as_os_str());
        let _key = EnvGuard::set("CF_API_KEY", std::ffi::OsStr::new(""));

        assert_eq!(
            provider_issue(&profile_with(PackSection::local(None))),
            None
        );
        assert_eq!(
            provider_issue(&profile_with(PackSection::curseforge(1))),
            None
        );

        // A stored key with no master key to decrypt it.
        let broken = ServerProfile {
            secrets: SecretsSection {
                api_key: Some("v1:AAAA".to_string()),
            },
            ..profile_with(PackSection::curseforge(1))
        };
        let issue = provider_issue(&broken).expect("a decrypt failure is a finding");
        assert_eq!(issue.severity, Severity::Error);
        assert!(
            issue
                .message
                .starts_with("CurseForge key could not be decrypted: "),
            "{}",
            issue.message
        );
        assert!(
            issue
                .message
                .ends_with("run 'packctl apikey --remove' and store the key again"),
            "{}",
            issue.message
        );

        // Modrinth needs no key; only an invalid API base fails setup.
        let modrinth = PackSection {
            provider: ProviderKind::Modrinth,
            project_id: Some("5FFgwNNP".to_string()),
            ..PackSection::local(None)
        };
        let _base = EnvGuard::set(
            crate::providers::modrinth::client::ENV_API_BASE,
            std::ffi::OsStr::new(""),
        );
        assert_eq!(provider_issue(&profile_with(modrinth.clone())), None);
        let _base = EnvGuard::set(
            crate::providers::modrinth::client::ENV_API_BASE,
            std::ffi::OsStr::new("http://api.example/v2"),
        );
        let issue = provider_issue(&profile_with(modrinth)).unwrap();
        assert!(
            issue
                .message
                .starts_with("pack provider 'modrinth' could not be set up: "),
            "{}",
            issue.message
        );
    }

    #[test]
    fn error_count_counts_only_errors() {
        assert_eq!(
            error_count(&[
                issue(Severity::Warning),
                issue(Severity::Error),
                issue(Severity::Error),
            ]),
            2
        );
    }

    fn issue(severity: Severity) -> ValidationIssue {
        ValidationIssue {
            severity,
            message: "finding".to_string(),
        }
    }

    #[test]
    fn issue_summary_counts_each_severity_separately() {
        assert_eq!(issue_summary(&[]), "0 warning(s), 0 error(s)");
        assert_eq!(
            issue_summary(&[issue(Severity::Warning), issue(Severity::Warning)]),
            "2 warning(s), 0 error(s)"
        );
        assert_eq!(
            issue_summary(&[issue(Severity::Error)]),
            "0 warning(s), 1 error(s)"
        );
        // Order must not matter: errors between warnings are still counted
        // as errors.
        assert_eq!(
            issue_summary(&[
                issue(Severity::Warning),
                issue(Severity::Error),
                issue(Severity::Warning),
                issue(Severity::Error),
                issue(Severity::Error),
            ]),
            "2 warning(s), 3 error(s)"
        );
    }
}
