//! Doctor command.
//!
//! Runs the environment checks against a server profile and reports the
//! findings. The command exits non-zero when any check fails so scripts and
//! remote invocations can react to the result (see design notes "Validation").

use crate::core::overlay::OverlayEngine;
use crate::core::updater::Updater;
use crate::core::validation::{Severity, ValidationIssue, has_errors, validate};
use crate::error::{PackError, Result};

/// Runs the environment checks for a server profile and prints the results.
pub async fn run(server: Option<&str>) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let overlay_files = OverlayEngine::new(profile.overlay.path.clone()).scan()?;
    let issues = validate(&profile, None, &overlay_files, updater.controller.as_ref()).await?;

    print_issues(&issues);
    println!("{}", issue_summary(&issues));

    if has_errors(&issues) {
        return Err(PackError::Validation("doctor found errors".into()));
    }

    println!("All checks passed.");
    Ok(())
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
