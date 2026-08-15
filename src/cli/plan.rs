//! `packctl plan` — show what an update would do without changing anything.

use crate::config::profile::ServerProfile;
use crate::core::planner::{OverlayChangeStatus, UpdatePlan};
use crate::core::updater::Updater;
use crate::core::validation::{Severity, validate};
use crate::error::Result;
use crate::providers::VersionSelector;

/// Prepares an update and renders the plan without mutating the server.
pub async fn run(server: Option<&str>, version: Option<&str>, verbose: bool) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let selector = version
        .map(|v| VersionSelector::Name(v.to_string()))
        .unwrap_or(VersionSelector::Latest);
    let prepared = updater.prepare_update(&selector).await?;

    print_plan(&profile, &prepared.plan, verbose)?;
    if prepared.plan.is_empty() {
        return Ok(());
    }

    print_server_section(&profile, &updater).await?;
    Ok(())
}

/// Shared renderer for the update preview, used by both `packctl plan` and
/// `packctl update` so the two commands never diverge.
pub(crate) fn print_plan(profile: &ServerProfile, plan: &UpdatePlan, verbose: bool) -> Result<()> {
    print!("{}", plan_summary(profile, plan, verbose));
    Ok(())
}

/// Renders the read-only server checks for a planned update.
///
/// Only ERROR-severity findings are shown; warnings are informational and do
/// not block a preview.
async fn print_server_section(profile: &ServerProfile, updater: &Updater) -> Result<()> {
    let issues = validate(profile, None, &[], updater.controller.as_ref()).await?;
    let errors: Vec<_> = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .collect();

    let mut lines = vec![String::new(), "Server".to_string()];
    if errors.is_empty() {
        lines.push("  ✓ server checks passed".to_string());
    } else {
        lines.extend(errors.iter().map(|issue| format!("  ✗ {}", issue.message)));
    }
    println!("{}", lines.join("\n"));
    Ok(())
}

/// Builds the full human-readable update preview from a plan.
fn plan_summary(profile: &ServerProfile, plan: &UpdatePlan, verbose: bool) -> String {
    if plan.is_empty() {
        return "No changes needed.\n".to_string();
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(profile.name.clone());

    let from = plan.from_version.as_deref().unwrap_or("none");
    lines.push(format!("{from} → {}", plan.to_version));
    lines.push(String::new());

    if plan.upstream_change_count() > 0 {
        lines.push("Upstream".to_string());
        if !plan.additions.is_empty() {
            lines.push(format!("  + {} files", plan.additions.len()));
        }
        if !plan.modifications.is_empty() {
            lines.push(format!("  ~ {} files", plan.modifications.len()));
        }
        if !plan.removals.is_empty() {
            lines.push(format!("  - {} files", plan.removals.len()));
        }
        lines.push(String::new());
    }

    let applied = plan
        .overlay_changes
        .iter()
        .filter(|change| change.status == OverlayChangeStatus::Applied)
        .count();
    let replaced_changed = plan
        .overlay_changes
        .iter()
        .filter(|change| change.status == OverlayChangeStatus::ReplacesChanged)
        .count();
    if applied > 0 || replaced_changed > 0 {
        lines.push("Overlay".to_string());
        if applied > 0 {
            lines.push(format!("  ✓ {applied} files"));
        }
        if replaced_changed > 0 {
            lines.push(format!(
                "  ⚠ {replaced_changed} replace files changed upstream"
            ));
        }
        lines.push(String::new());
    }

    for notice in &plan.notices {
        lines.push(notice.message.clone());
        lines.push(String::new());
    }

    if verbose {
        lines.push("Files".to_string());
        for change in &plan.additions {
            lines.push(format!("  + {}", change.rel_path.display()));
        }
        for change in &plan.modifications {
            lines.push(format!("  ~ {}", change.rel_path.display()));
        }
        for change in &plan.removals {
            lines.push(format!("  - {}", change.rel_path.display()));
        }
        for change in &plan.overlay_changes {
            let note = if change.status == OverlayChangeStatus::ReplacesChanged {
                " (upstream changed)"
            } else {
                ""
            };
            lines.push(format!("  ~ {} (overlay){note}", change.rel_path.display()));
        }
        lines.push(String::new());
    }

    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::profile::{
        ControllerKind, ControllerSection, OverlaySection, PackSection, ProviderKind,
        SecretsSection, ServerSection,
    };
    use crate::core::planner::{ChangeKind, FileChange, OverlayChange, PlanNotice};

    fn fixture_profile() -> ServerProfile {
        ServerProfile {
            name: "ATM10".to_string(),
            server: ServerSection {
                root: PathBuf::from("/srv/atm10"),
            },
            pack: PackSection {
                provider: ProviderKind::CurseForge,
                project_id: 925200,
                slug: Some("atm10".to_string()),
                archive: None,
            },
            overlay: OverlaySection {
                path: PathBuf::from("/srv/overlay"),
            },
            controller: ControllerSection {
                kind: ControllerKind::Command,
                instance: None,
                command: None,
            },
            secrets: SecretsSection::default(),
        }
    }

    fn fixture_plan() -> UpdatePlan {
        UpdatePlan {
            from_version: Some("4.11".to_string()),
            from_id: Some("11".to_string()),
            to_version: "4.12".to_string(),
            to_id: "12".to_string(),
            additions: vec![FileChange {
                rel_path: PathBuf::from("mods/upstream-a-new.jar"),
                kind: ChangeKind::Add,
                source: None,
                sha256: None,
            }],
            modifications: vec![FileChange {
                rel_path: PathBuf::from("mods/changed.jar"),
                kind: ChangeKind::Replace,
                source: None,
                sha256: None,
            }],
            removals: vec![FileChange {
                rel_path: PathBuf::from("mods/upstream-old.jar"),
                kind: ChangeKind::Remove,
                source: None,
                sha256: None,
            }],
            overlay_changes: vec![
                OverlayChange {
                    rel_path: PathBuf::from("mods/grieflogger.jar"),
                    status: OverlayChangeStatus::Applied,
                },
                OverlayChange {
                    rel_path: PathBuf::from("config/main.conf"),
                    status: OverlayChangeStatus::ReplacesChanged,
                },
            ],
            notices: vec![PlanNotice {
                path: Some(PathBuf::from("config/main.conf")),
                message: "Overlay conflict notice".to_string(),
            }],
        }
    }

    #[test]
    fn plan_summary_reports_counts() {
        let summary = plan_summary(&fixture_profile(), &fixture_plan(), false);

        assert!(summary.contains("ATM10"));
        assert!(summary.contains("4.11 → 4.12"));
        assert!(summary.contains("Upstream"));
        assert!(summary.contains("  + 1 files"));
        assert!(summary.contains("  ~ 1 files"));
        assert!(summary.contains("  - 1 files"));
        assert!(summary.contains("Overlay"));
        assert!(summary.contains("  ✓ 1 files"));
        assert!(summary.contains("  ⚠ 1 replace files changed upstream"));
    }

    #[test]
    fn plan_summary_verbose_lists_files_and_overlay_notes() {
        let summary = plan_summary(&fixture_profile(), &fixture_plan(), true);

        assert!(summary.contains("Files"));
        assert!(summary.contains("  + mods/upstream-a-new.jar"));
        assert!(summary.contains("  ~ mods/changed.jar"));
        assert!(summary.contains("  - mods/upstream-old.jar"));
        assert!(summary.contains("  ~ mods/grieflogger.jar (overlay)"));
        assert!(
            summary.contains("  ~ config/main.conf (overlay) (upstream changed)"),
            "summary: {summary}"
        );
    }

    #[test]
    fn plan_summary_prints_empty_plan_message() {
        let mut plan = fixture_plan();
        plan.additions.clear();
        plan.modifications.clear();
        plan.removals.clear();
        plan.overlay_changes.clear();

        let summary = plan_summary(&fixture_profile(), &plan, false);

        assert!(summary.contains("No changes needed."));
        assert!(!summary.contains("Upstream"));
        assert!(!summary.contains("Overlay"));
    }

    #[test]
    fn plan_summary_hides_zero_count_rows() {
        let mut plan = fixture_plan();
        plan.removals.clear();
        plan.overlay_changes = vec![OverlayChange {
            rel_path: PathBuf::from("mods/grieflogger.jar"),
            status: OverlayChangeStatus::Applied,
        }];

        let summary = plan_summary(&fixture_profile(), &plan, false);

        assert!(!summary.contains("  - "), "summary: {summary}");
        assert!(summary.contains("  ✓ 1 files"));
        assert!(
            !summary.contains("replace files changed upstream"),
            "summary: {summary}"
        );
    }

    #[test]
    fn plan_summary_uses_none_when_no_from_version() {
        let mut plan = fixture_plan();
        plan.from_version = None;

        let summary = plan_summary(&fixture_profile(), &plan, false);

        assert!(summary.contains("none → 4.12"));
    }
}
