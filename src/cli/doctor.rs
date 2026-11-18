//! Doctor command.
//!
//! Runs the environment checks against a server profile and reports the
//! findings. The command exits non-zero when any check fails so scripts and
//! remote invocations can react to the result (see design notes "Validation").

use super::validate::{error_count, issue_summary, provider_issue};
use std::path::Path;

use crate::config::profile::{ProviderKind, ServerProfile};
use crate::core::loader::detect_installed_loader;
use crate::core::overlay::OverlayEngine;
use crate::core::validation::{Severity, ValidationIssue, has_errors, validate};
use crate::error::{PackError, Result};
use crate::providers::curseforge::client::CfClient;

/// Runs the environment checks for a server profile and prints the results.
pub async fn run(server: Option<&str>) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    // The controller and the pack provider are built separately, so a
    // provider that cannot be set up (for example an API key that no longer
    // decrypts) is reported as a finding instead of stopping every check.
    let controller = crate::controllers::from_profile(&profile.controller)?;
    let mut issues: Vec<ValidationIssue> = provider_issue(&profile).into_iter().collect();
    if profile.pack.provider == ProviderKind::CurseForge {
        issues.extend(check_curseforge_endpoint(&profile).await);
    }
    if profile.pack.provider == ProviderKind::Modrinth {
        outln!("{}", detected_loader_line(&profile.server.root));
    }

    // A state file that cannot be read is reported by `validate`; doctor
    // only needs to know whether packctl has recorded this server yet.
    if let Ok(state) = crate::core::state::StateStore::at(&profile.server.root)?.load() {
        issues.extend(unmanaged_server_issue(&profile.server.root, &state, server));
    }

    let overlay_files = OverlayEngine::new(profile.overlay.path.clone()).scan()?;
    issues.extend(validate(&profile, None, &overlay_files, controller.as_ref()).await?);

    print_issues(&issues);
    outln!("{}", issue_summary(&issues));

    if has_errors(&issues) {
        return Err(PackError::Validation(format!(
            "{} error(s) found",
            error_count(&issues)
        )));
    }

    outln!("All checks passed.");
    Ok(())
}

/// Prints the CurseForge endpoint in use and probes it with a project
/// lookup. An endpoint that cannot be built (a key that no longer decrypts)
/// is already reported by [`provider_issue`]; an endpoint that does not
/// answer becomes a warning.
async fn check_curseforge_endpoint(profile: &ServerProfile) -> Option<ValidationIssue> {
    let endpoint = profile.curseforge_endpoint().ok()?;
    outln!("CurseForge endpoint: {}", endpoint.describe());
    let project_id = profile
        .pack
        .project_id
        .as_deref()?
        .trim()
        .parse::<u32>()
        .ok()?;
    let base = endpoint.base_url.clone();
    let client = CfClient::new(endpoint);
    match client.probe(project_id).await {
        Ok(()) => None,
        Err(err) => Some(ValidationIssue {
            severity: Severity::Warning,
            message: format!("CurseForge endpoint {base} is not usable: {err}"),
        }),
    }
}

/// `Detected loader: NeoForge 21.1.77`, or `Detected loader: none`.
/// packctl does not install loaders, so admins check this against what the
/// pack requires.
fn detected_loader_line(server_root: &Path) -> String {
    match detect_installed_loader(server_root) {
        Some(detected) => format!("Detected loader: {}", detected.describe()),
        None => "Detected loader: none".to_string(),
    }
}

/// A warning for a server that already has a pack installed by hand but no
/// packctl record: its first update would treat every admin change as
/// something to delete or overwrite, so it should be adopted first.
/// `server` is the profile name the user typed, if any.
fn unmanaged_server_issue(
    server_root: &Path,
    state: &crate::core::state::InstalledState,
    server: Option<&str>,
) -> Option<ValidationIssue> {
    let has_files = |dir: &str| {
        walkdir::WalkDir::new(server_root.join(dir))
            .into_iter()
            .flatten()
            .any(|entry| entry.file_type().is_file())
    };
    (state.is_empty() && (has_files("mods") || has_files("config"))).then(|| ValidationIssue {
        severity: Severity::Warning,
        message: format!(
            "packctl has no record of this server yet; run '{} --as <version>' before the \
             first update",
            crate::cli::adopt::adopt_command(server)
        ),
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;
    use crate::config::profile::{
        ControllerKind, ControllerSection, OverlaySection, PackSection, SecretsSection,
        ServerSection,
    };
    use crate::providers::curseforge::endpoint::{ENV_API_BASE, ENV_PROXY_SEND_KEY};
    use crate::providers::curseforge::models::fixtures;
    use crate::testutil::http_server::{Route, serve};
    use crate::testutil::{EnvGuard, env_lock};

    fn profile() -> ServerProfile {
        ServerProfile {
            name: "t".to_string(),
            server: ServerSection {
                root: PathBuf::from("/srv/mc"),
            },
            pack: PackSection::curseforge(925200),
            overlay: OverlaySection {
                path: PathBuf::from("/srv/mc/overlay"),
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
    fn endpoint_probe_warns_only_when_the_endpoint_fails() {
        let _lock = env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("PACKCTL_HOME", home.path().as_os_str());
        let _key = EnvGuard::set("CF_API_KEY", OsStr::new(""));
        let _send = EnvGuard::set(ENV_PROXY_SEND_KEY, OsStr::new(""));
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let healthy = runtime.block_on(serve(vec![Route::json(
            "/v1/mods/925200",
            fixtures::MOD_925200,
        )]));
        let _base = EnvGuard::set(ENV_API_BASE, OsStr::new(&healthy));
        assert_eq!(
            runtime.block_on(check_curseforge_endpoint(&profile())),
            None
        );

        let broken = runtime.block_on(serve(vec![Route::status("/v1/mods/925200", 502)]));
        let _base = EnvGuard::set(ENV_API_BASE, OsStr::new(&broken));
        let issue = runtime
            .block_on(check_curseforge_endpoint(&profile()))
            .expect("a failing endpoint is a finding");
        assert_eq!(issue.severity, Severity::Warning);
        assert!(
            issue
                .message
                .starts_with(&format!("CurseForge endpoint {broken} is not usable: ")),
            "{}",
            issue.message
        );
    }
    #[test]
    fn detected_loader_line_names_the_loader_or_none() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(detected_loader_line(root.path()), "Detected loader: none");
        std::fs::create_dir_all(
            root.path()
                .join("libraries/net/fabricmc/fabric-loader/0.16.10"),
        )
        .unwrap();
        assert_eq!(
            detected_loader_line(root.path()),
            "Detected loader: Fabric 0.16.10"
        );
    }
    #[test]
    fn unmanaged_server_with_mods_is_told_to_adopt() {
        let root = tempfile::tempdir().unwrap();
        let empty = crate::core::state::InstalledState::default();
        assert_eq!(unmanaged_server_issue(root.path(), &empty, None), None);

        std::fs::create_dir_all(root.path().join("config/sub")).unwrap();
        std::fs::write(root.path().join("config/sub/a.toml"), b"a").unwrap();
        let issue = unmanaged_server_issue(root.path(), &empty, None).expect("a warning");
        assert_eq!(issue.severity, Severity::Warning);
        assert!(issue.message.contains("'packctl adopt --as <version>'"));
        // A named (global) profile is named in the hint too.
        let named = unmanaged_server_issue(root.path(), &empty, Some("my server")).unwrap();
        assert!(
            named
                .message
                .contains("'packctl adopt 'my server' --as <version>'"),
            "{}",
            named.message
        );

        let recorded = crate::core::state::InstalledState {
            installed_version: Some("1.0".to_string()),
            ..Default::default()
        };
        assert_eq!(unmanaged_server_issue(root.path(), &recorded, None), None);
    }
}
