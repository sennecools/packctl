//! Update validation.
//!
//! Validation runs before an update mutates the live server and checks the
//! conditions an update depends on: the server root exists and is writable,
//! expected entry files are present, the overlay is where it should be, enough
//! free disk space exists for the prepared upstream, and the controller is
//! usable (see design notes "Validation").
//!
//! Findings are reported as [`ValidationIssue`]s with a [`Severity`]. Hard
//! conditions that are broken become `Error` issues; soft conditions become
//! `Warning`s. Genuine internal failures (for example an invariant that is
//! broken) surface as `Err` rather than as an issue.
//!

use std::path::Path;

use crate::config::profile::ServerProfile;
use crate::controllers::ServerController;
use crate::core::overlay::OverlayFile;
use crate::error::Result;
use crate::providers::PreparedPack;

/// How severe a validation finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

/// A single validation finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub severity: Severity,
    pub message: String,
}

/// Returns true when at least one issue must block the update.
pub fn has_errors(issues: &[ValidationIssue]) -> bool {
    issues.iter().any(|issue| issue.severity == Severity::Error)
}

/// Probes whether `dir` is writable by creating and removing a probe file.
///
/// Returns false when the directory does not exist or the probe cannot be
/// created or removed.
pub(crate) fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".packctl-write-probe");
    std::fs::File::create(&probe).is_ok() && std::fs::remove_file(&probe).is_ok()
}

/// Parses the available space in 1K blocks from `df -P` output.
///
/// `df -P` emits a header line followed by one line per filesystem; the fourth
/// column holds the available blocks. Returns `None` when the output cannot be
/// parsed.
pub(crate) fn parse_df_available(output: &str) -> Option<u64> {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Filesystem") {
            continue;
        }
        return trimmed.split_whitespace().nth(3)?.parse().ok();
    }
    None
}

/// Runs the validation checks against a profile and returns the findings.
///
/// `prepared` is the staged upstream pack when one was constructed; disk-space
/// checking is only meaningful then. `overlay_files` is accepted for API
/// symmetry with the executor. `controller` must be reachable for the update.
pub async fn validate(
    profile: &ServerProfile,
    prepared: Option<&PreparedPack>,
    overlay_files: &[OverlayFile],
    controller: &dyn ServerController,
) -> Result<Vec<ValidationIssue>> {
    let _ = overlay_files;
    let mut issues = Vec::new();
    let root = &profile.server.root;

    let root_is_dir = match std::fs::metadata(root) {
        Ok(meta) if meta.is_dir() => true,
        Ok(_) => {
            issues.push(error_issue(format!(
                "server root '{}' is not a directory",
                root.display()
            )));
            false
        }
        Err(_) => {
            issues.push(error_issue(format!(
                "server root '{}' does not exist",
                root.display()
            )));
            false
        }
    };

    if root_is_dir {
        if !is_writable(root) {
            issues.push(error_issue(format!(
                "server root '{}' is not writable",
                root.display()
            )));
        }
        if !has_entry_files(root) {
            issues.push(warning_issue(format!(
                "no expected entry files found in server root '{}' (expected one of {}, forge-*.jar, neoforge-*.jar)",
                root.display(),
                ENTRY_FILES.join(", ")
            )));
        }
    }

    if !profile.overlay.path.is_dir() {
        issues.push(warning_issue(format!(
            "overlay configured but not found: '{}'",
            profile.overlay.path.display()
        )));
    }

    if let Some(prepared) = prepared {
        check_disk_space(root, prepared, &mut issues);
    }

    match controller.status().await {
        Ok(_) => {}
        Err(err) => issues.push(error_issue(format!("controller is not usable: {err}"))),
    }

    Ok(issues)
}

/// Entry files that indicate an unpacked Minecraft server at the top level.
const ENTRY_FILES: [&str; 4] = [
    "server.jar",
    "run.sh",
    "start.sh",
    "fabric-server-launch.jar",
];

/// Returns true when the server root contains any expected entry file.
///
/// `forge-*.jar` and `neoforge-*.jar` are matched as top-level prefixes.
fn has_entry_files(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            "server.jar" | "run.sh" | "start.sh" | "fabric-server-launch.jar"
        ) || (name.ends_with(".jar")
            && (name.starts_with("forge-") || name.starts_with("neoforge-")))
        {
            return true;
        }
    }
    false
}

/// Adds a disk-space finding when the prepared pack cannot fit.
///
/// The required space is the total prepared size plus 15% for the overlay and
/// filesystem slack. Free space comes from `df -P` on the server root's
/// filesystem; an unparseable result is a warning rather than a hard error.
fn check_disk_space(root: &Path, prepared: &PreparedPack, issues: &mut Vec<ValidationIssue>) {
    let total: u64 = prepared.files.iter().map(|file| file.size).sum();
    let required = total + total / 100 * 15;
    match free_space(root) {
        Some(free) if free < required => {
            issues.push(error_issue(format!(
                "insufficient free disk space on '{}': about {} MiB needed, {} MiB available",
                root.display(),
                bytes_to_mib(required),
                bytes_to_mib(free)
            )));
        }
        Some(_) => {}
        None => {
            issues.push(warning_issue(format!(
                "could not determine free disk space on '{}'",
                root.display()
            )));
        }
    }
}

/// Returns the free space in bytes on the filesystem containing `root`.
fn free_space(root: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .arg("-P")
        .arg(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_df_available(&String::from_utf8_lossy(&output.stdout))?.checked_mul(1024)
}

fn bytes_to_mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

fn error_issue(message: String) -> ValidationIssue {
    ValidationIssue {
        severity: Severity::Error,
        message,
    }
}

fn warning_issue(message: String) -> ValidationIssue {
    ValidationIssue {
        severity: Severity::Warning,
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::profile::{
        ControllerKind, ControllerSection, OverlaySection, PackSection, ProviderKind, ServerSection,
    };
    use crate::controllers::ServerStatus;
    use crate::error::PackError;
    use crate::providers::{PackVersion, PreparedFile};

    struct OkController;
    #[async_trait::async_trait]
    impl ServerController for OkController {
        async fn status(&self) -> Result<ServerStatus> {
            Ok(ServerStatus::Stopped)
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn start(&self) -> Result<()> {
            Ok(())
        }
    }

    struct UnknownController;
    #[async_trait::async_trait]
    impl ServerController for UnknownController {
        async fn status(&self) -> Result<ServerStatus> {
            Ok(ServerStatus::Unknown)
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn start(&self) -> Result<()> {
            Ok(())
        }
    }

    struct FailingController;
    #[async_trait::async_trait]
    impl ServerController for FailingController {
        async fn status(&self) -> Result<ServerStatus> {
            Err(PackError::Other("controller is down".to_string()))
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn start(&self) -> Result<()> {
            Ok(())
        }
    }

    fn profile(root: &Path, overlay: &Path) -> ServerProfile {
        ServerProfile {
            name: "test-server".to_string(),
            server: ServerSection {
                root: root.to_path_buf(),
            },
            pack: PackSection {
                provider: ProviderKind::CurseForge,
                project_id: 1,
                slug: None,
            },
            overlay: OverlaySection {
                path: overlay.to_path_buf(),
            },
            controller: ControllerSection {
                kind: ControllerKind::Command,
                instance: None,
                command: None,
            },
            secrets: crate::config::profile::SecretsSection::default(),
        }
    }

    #[tokio::test]
    async fn missing_root_yields_error_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-server");
        let profile = profile(&missing, &tmp.path().join("overlay"));

        let issues = validate(&profile, None, &[], &OkController).await.unwrap();

        assert!(has_errors(&issues));
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("does not exist")),
            "issues: {issues:#?}"
        );
    }

    #[tokio::test]
    async fn clean_server_passes_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("server.jar"), b"data").unwrap();
        let overlay = tmp.path().join("overlay");
        std::fs::create_dir_all(&overlay).unwrap();

        let profile = profile(&root, &overlay);
        let issues = validate(&profile, None, &[], &OkController).await.unwrap();

        assert!(issues.is_empty(), "issues: {issues:#?}");
    }

    #[test]
    fn is_writable_probes_temp_dir_and_rejects_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(is_writable(tmp.path()));
        assert!(!is_writable(&tmp.path().join("no-such-dir")));
    }

    #[tokio::test]
    async fn missing_entry_files_yield_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("mods")).unwrap();
        std::fs::write(root.join("mods/whatever.jar"), b"x").unwrap();
        let overlay = tmp.path().join("overlay");
        std::fs::create_dir_all(&overlay).unwrap();

        let profile = profile(&root, &overlay);
        let issues = validate(&profile, None, &[], &UnknownController)
            .await
            .unwrap();

        let entry = issues.iter().find(|issue| {
            issue.severity == Severity::Warning && issue.message.contains("entry files")
        });
        assert!(entry.is_some(), "issues: {issues:#?}");
    }

    #[test]
    fn parse_df_available_reads_available_column() {
        let output = "Filesystem     1K-blocks     Used Available Use% Mounted on\n\
                      /dev/sda1      10485760   5242880   5242880  50% /srv\n";
        assert_eq!(parse_df_available(output), Some(5242880));

        assert_eq!(parse_df_available(""), None);
        assert_eq!(
            parse_df_available("Filesystem     1K-blocks     Used Available Use% Mounted on\n"),
            None
        );
        assert_eq!(parse_df_available("garbage without numbers\n"), None);
    }

    #[test]
    fn df_blocks_are_converted_to_bytes() {
        assert_eq!(bytes_to_mib(524_288 * 1024), 512);
    }

    #[tokio::test]
    async fn failing_controller_yields_error_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("server.jar"), b"data").unwrap();
        let overlay = tmp.path().join("overlay");
        std::fs::create_dir_all(&overlay).unwrap();

        let profile = profile(&root, &overlay);
        let issues = validate(&profile, None, &[], &FailingController)
            .await
            .unwrap();

        assert!(has_errors(&issues));
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("controller is not usable")),
            "issues: {issues:#?}"
        );
    }

    #[tokio::test]
    async fn validate_returns_ok_with_issues_when_checks_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();
        let overlay = tmp.path().join("missing-overlay");

        let profile = profile(&root, &overlay);
        let issues = validate(&profile, None, &[], &FailingController)
            .await
            .unwrap();

        assert!(!issues.is_empty());
        assert!(has_errors(&issues));
    }

    #[tokio::test]
    async fn prepared_pack_with_sufficient_space_passes_disk_check() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("server.jar"), b"data").unwrap();
        let overlay = tmp.path().join("overlay");
        std::fs::create_dir_all(&overlay).unwrap();

        let prepared = PreparedPack {
            name: "test".to_string(),
            version: PackVersion {
                id: "1".to_string(),
                name: "1".to_string(),
                file_id: None,
                released: None,
            },
            root: tmp.path().join("staging"),
            files: vec![PreparedFile {
                rel_path: PathBuf::from("mods/a.jar"),
                size: 1024,
                sha256: "abc".to_string(),
            }],
        };

        let profile = profile(&root, &overlay);
        let issues = validate(&profile, Some(&prepared), &[], &OkController)
            .await
            .unwrap();

        assert!(
            !issues
                .iter()
                .any(|issue| issue.message.contains("insufficient free disk")),
            "issues: {issues:#?}"
        );
    }
}
