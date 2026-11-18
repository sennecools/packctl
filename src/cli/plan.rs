//! `packctl plan` — show what an update would do without changing anything.

use std::path::Path;

use crate::cli::adopt::adopt_command;
use crate::cli::create::shell_quote;
use crate::config::profile::ServerProfile;
use crate::core::planner::{OverlayChangeStatus, PlanNotice, UpdatePlan};
use crate::core::updater::{FirstUpdateRisk, PreparedUpdate, Updater};
use crate::core::validation::{Severity, validate};
use crate::error::Result;
use crate::providers::VersionSelector;

/// Prepares an update and renders the plan without mutating the server.
pub async fn run(
    server: Option<&str>,
    version: Option<&str>,
    verbose: bool,
    json: bool,
) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let selector = version
        .map(|v| VersionSelector::Query(v.to_string()))
        .unwrap_or(VersionSelector::Latest);
    let prepared = updater.prepare_update(&selector).await?;

    if json {
        let text = plan_json(&profile.name, &prepared.plan, prepared.first_update_risk())?;
        outln!("{text}");
        return Ok(());
    }

    let summary = render_plan(&profile, &prepared, server, verbose);
    out!("{summary}");
    if prepared.plan.is_empty() {
        return Ok(());
    }

    print_server_section(&profile, &updater, &summary).await?;
    Ok(())
}

/// The `plan --json` document: the profile name, the full plan, and the
/// first-update risk (`null` when the server is managed or nothing is at
/// risk).
fn plan_json(
    profile: &str,
    plan: &UpdatePlan,
    first_update_risk: Option<FirstUpdateRisk>,
) -> Result<String> {
    #[derive(serde::Serialize)]
    struct PlanJson<'a> {
        profile: &'a str,
        plan: &'a UpdatePlan,
        first_update_risk: Option<FirstUpdateRisk>,
    }
    serde_json::to_string_pretty(&PlanJson {
        profile,
        plan,
        first_update_risk,
    })
    .map_err(|err| crate::error::PackError::Parse(format!("failed to serialize plan: {err}")))
}

/// Shared renderer for the update preview, used by both `packctl plan` and
/// `packctl update` so the two commands never diverge. `server` is the
/// profile name the user typed, if any, for the hints.
pub(crate) fn render_plan(
    profile: &ServerProfile,
    prepared: &PreparedUpdate,
    server: Option<&str>,
    verbose: bool,
) -> String {
    let mut summary = plan_summary(
        profile,
        &prepared.plan,
        verbose,
        prepared.needs_state_commit(),
    );
    if let Some(risk) = prepared.first_update_risk() {
        summary.push_str(&first_update_block(risk, &adopt_command(server)));
    }
    summary
}

/// The warning for the first update of a server packctl has no record of.
/// `adopt` is how the user runs `packctl adopt` for this server.
fn first_update_block(risk: FirstUpdateRisk, adopt: &str) -> String {
    format!(
        "⚠ First update of an unmanaged server\n  {} not in the pack would be deleted, {} \
         that {} from the pack would be overwritten.\n  Adopt it first: {adopt} --as \
         <version>\n\n",
        count(risk.removals, "file", "files"),
        count(risk.overwrites, "file", "files"),
        if risk.overwrites == 1 {
            "differs"
        } else {
            "differ"
        },
    )
}

/// Renders the read-only server checks for a planned update.
///
/// Only ERROR-severity findings are shown; warnings are informational and do
/// not block a preview.
async fn print_server_section(
    profile: &ServerProfile,
    updater: &Updater,
    summary: &str,
) -> Result<()> {
    let issues = validate(profile, None, &[], updater.controller.as_ref()).await?;
    let errors: Vec<String> = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .map(|issue| issue.message.clone())
        .collect();
    out!("{}", server_section(&errors, summary));
    Ok(())
}

/// Builds the "Server" block that follows `summary`, separated from it by
/// exactly one blank line.
fn server_section(errors: &[String], summary: &str) -> String {
    let mut lines = Vec::new();
    if !summary.is_empty() && !summary.ends_with("\n\n") {
        lines.push(String::new());
    }
    lines.push("Server".to_string());
    if errors.is_empty() {
        lines.push("  ✓ server checks passed".to_string());
    } else {
        lines.extend(errors.iter().map(|error| format!("  ✗ {error}")));
    }
    lines.join("\n") + "\n"
}

/// `1 file` / `14 files`.
fn count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// Most paths listed in a warning block unless `--verbose` is given.
const MAX_LISTED: usize = 10;

fn push_paths(lines: &mut Vec<String>, paths: &[&Path], verbose: bool) {
    let shown = if verbose { paths.len() } else { MAX_LISTED };
    for path in paths.iter().take(shown) {
        lines.push(format!("  {}", path.display()));
    }
    if paths.len() > shown {
        lines.push(format!(
            "  … and {} more (use --verbose to list all)",
            paths.len() - shown
        ));
    }
}

/// Builds the full human-readable update preview from a plan.
///
/// `state_commit` says whether the update would record a new state; a plan
/// without file work then reads as "record only" instead of "no changes".
fn plan_summary(
    profile: &ServerProfile,
    plan: &UpdatePlan,
    verbose: bool,
    state_commit: bool,
) -> String {
    if plan.is_empty() {
        return empty_plan_summary(plan, verbose, state_commit);
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(profile.name.clone());

    let from = plan.from_version.as_deref().unwrap_or("none");
    lines.push(format!("{from} → {}", plan.to_version));
    if let Some(requirements) = &plan.requirements {
        let described = requirements.describe();
        if !described.is_empty() {
            lines.push(format!("Requires {described}"));
        }
    }
    lines.push(String::new());

    if plan.upstream_change_count() > 0 {
        lines.push("Upstream".to_string());
        if !plan.additions.is_empty() {
            lines.push(format!(
                "  + {}",
                count(plan.additions.len(), "file", "files")
            ));
        }
        if !plan.modifications.is_empty() {
            lines.push(format!(
                "  ~ {}",
                count(plan.modifications.len(), "file", "files")
            ));
        }
        if !plan.removals.is_empty() {
            lines.push(format!(
                "  - {}",
                count(plan.removals.len(), "file", "files")
            ));
        }
        lines.push(String::new());
    }

    let to_apply = plan
        .overlay_changes
        .iter()
        .filter(|change| change.pending && change.status != OverlayChangeStatus::ReplacesChanged)
        .count();
    let replaced_changed = plan
        .overlay_changes
        .iter()
        .filter(|change| change.status == OverlayChangeStatus::ReplacesChanged)
        .count();
    if to_apply > 0 || replaced_changed > 0 {
        lines.push("Overlay".to_string());
        if to_apply > 0 {
            lines.push(format!("  ✓ {} to apply", count(to_apply, "file", "files")));
        }
        if replaced_changed > 0 {
            let what = if replaced_changed == 1 {
                "overlay file replaces a file that changed upstream"
            } else {
                "overlay files replace files that changed upstream"
            };
            lines.push(format!("  ⚠ {replaced_changed} {what}"));
        }
        lines.push(String::new());
    }

    if !plan.unmanaged_removals.is_empty() {
        lines.push(format!(
            "⚠ {} packctl never installed would be deleted",
            count(plan.unmanaged_removals.len(), "file", "files")
        ));
        let paths: Vec<&Path> = plan
            .unmanaged_removals
            .iter()
            .map(|p| p.as_path())
            .collect();
        push_paths(&mut lines, &paths, verbose);
        lines.push(
            "  To keep them, copy them into the overlay or add .packctlignore rules.".to_string(),
        );
        lines.push("  To delete them, run 'packctl update' with --force-sync.".to_string());
        lines.push(String::new());
    }

    if !plan.unmanaged_overwrites.is_empty() {
        lines.push(format!(
            "⚠ The pack would overwrite {} packctl never installed",
            count(plan.unmanaged_overwrites.len(), "file", "files")
        ));
        let paths: Vec<&Path> = plan
            .unmanaged_overwrites
            .iter()
            .map(|p| p.as_path())
            .collect();
        push_paths(&mut lines, &paths, verbose);
        lines.push(
            "  To keep your version, copy it into the overlay or add .packctlignore rules."
                .to_string(),
        );
        lines.push("  To overwrite them, run 'packctl update' with --force-sync.".to_string());
        lines.push(String::new());
    }

    let drift = plan.live_drift_paths();
    if !drift.is_empty() {
        lines.push(format!(
            "⚠ The overlay would overwrite {} changed outside packctl",
            count(drift.len(), "live file", "live files")
        ));
        push_paths(&mut lines, &drift, verbose);
        lines.push("  To keep the live version, copy it back into the overlay:".to_string());
        for path in drift
            .iter()
            .take(if verbose { drift.len() } else { MAX_LISTED })
        {
            lines.push(format!(
                "    cp -- {} {}",
                shell_quote(&profile.server.root.join(path).display().to_string()),
                shell_quote(&profile.overlay.path.join(path).display().to_string())
            ));
        }
        lines.push("  To overwrite them, run 'packctl update' with --force-sync.".to_string());
        lines.push(String::new());
    }

    for notice in shown_notices(plan, verbose) {
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
        lines.extend(overlay_file_lines(plan));
        lines.push(String::new());
    }

    lines.join("\n") + "\n"
}

/// The notices to print: all of them with `--verbose`, else those that are
/// not verbose-only.
fn shown_notices(plan: &UpdatePlan, verbose: bool) -> impl Iterator<Item = &PlanNotice> {
    plan.notices
        .iter()
        .filter(move |notice| verbose || !notice.verbose_only)
}

/// The preview for a plan without file work: notices, and with `--verbose`
/// the overlay files that are already in place.
fn empty_plan_summary(plan: &UpdatePlan, verbose: bool, state_commit: bool) -> String {
    let mut lines = vec![if state_commit {
        format!(
            "No file changes needed; 'packctl update' would record {} (id {}) as installed.",
            plan.to_version, plan.to_id
        )
    } else {
        "No changes needed.".to_string()
    }];
    for notice in shown_notices(plan, verbose) {
        lines.push(String::new());
        lines.push(notice.message.clone());
    }
    if verbose && !plan.overlay_changes.is_empty() {
        lines.push(String::new());
        lines.push("Files".to_string());
        lines.extend(overlay_file_lines(plan));
    }
    lines.join("\n") + "\n"
}

/// One line per overlay file for the verbose file list.
fn overlay_file_lines(plan: &UpdatePlan) -> Vec<String> {
    plan.overlay_changes
        .iter()
        .map(|change| {
            if !change.pending {
                return format!("  = {} (overlay, in sync)", change.rel_path.display());
            }
            let note = if change.status == OverlayChangeStatus::ReplacesChanged {
                " (upstream changed)"
            } else {
                ""
            };
            format!("  ~ {} (overlay){note}", change.rel_path.display())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::profile::{
        ControllerKind, ControllerSection, OverlaySection, PackSection, SecretsSection,
        ServerSection,
    };
    use crate::core::planner::{ChangeKind, FileChange, OverlayChange, PlanNotice};

    fn fixture_profile() -> ServerProfile {
        ServerProfile {
            name: "ATM10".to_string(),
            server: ServerSection {
                root: PathBuf::from("/srv/atm10"),
            },
            pack: PackSection {
                slug: Some("atm10".to_string()),
                ..PackSection::curseforge(925200)
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

    fn overlay_change(rel: &str, status: OverlayChangeStatus, pending: bool) -> OverlayChange {
        OverlayChange {
            rel_path: PathBuf::from(rel),
            status,
            pending,
            live_drift: false,
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
                overlay_change("mods/grieflogger.jar", OverlayChangeStatus::Applied, true),
                overlay_change(
                    "config/main.conf",
                    OverlayChangeStatus::ReplacesChanged,
                    true,
                ),
            ],
            notices: vec![PlanNotice {
                path: Some(PathBuf::from("config/main.conf")),
                verbose_only: false,
                message: "Overlay conflict notice".to_string(),
            }],
            unmanaged_removals: Vec::new(),
            unmanaged_overwrites: Vec::new(),
            requirements: None,
        }
    }

    fn empty_plan() -> UpdatePlan {
        let mut plan = fixture_plan();
        plan.additions.clear();
        plan.modifications.clear();
        plan.removals.clear();
        plan.overlay_changes.clear();
        plan.notices.clear();
        plan
    }

    #[test]
    fn plan_summary_reports_counts() {
        let summary = plan_summary(&fixture_profile(), &fixture_plan(), false, true);

        assert!(summary.contains("ATM10"));
        assert!(summary.contains("4.11 → 4.12"));
        assert!(summary.contains("Upstream"));
        assert!(summary.contains("  + 1 file\n"), "{summary}");
        assert!(summary.contains("  ~ 1 file\n"), "{summary}");
        assert!(summary.contains("  - 1 file\n"), "{summary}");
        assert!(summary.contains("Overlay"));
        assert!(summary.contains("  ✓ 1 file to apply"), "{summary}");
        assert!(
            summary.contains("  ⚠ 1 overlay file replaces a file that changed upstream"),
            "{summary}"
        );
    }

    #[test]
    fn requires_line_rendered_when_requirements_present() {
        use crate::providers::{LoaderKind, LoaderRequirement, PackRequirements};

        let summary = plan_summary(&fixture_profile(), &fixture_plan(), false, true);
        assert!(!summary.contains("Requires"), "{summary}");

        let mut plan = fixture_plan();
        plan.requirements = Some(PackRequirements {
            minecraft: Some("1.21.1".to_string()),
            loader: Some(LoaderRequirement {
                kind: LoaderKind::NeoForge,
                version: "21.1.77".to_string(),
            }),
        });
        let summary = plan_summary(&fixture_profile(), &plan, false, true);
        assert!(
            summary.contains("4.11 → 4.12\nRequires Minecraft 1.21.1 · NeoForge 21.1.77\n"),
            "{summary}"
        );

        // Missing parts are left out.
        plan.requirements = Some(PackRequirements {
            minecraft: Some("1.20.1".to_string()),
            loader: None,
        });
        let summary = plan_summary(&fixture_profile(), &plan, false, true);
        assert!(
            summary.contains("4.11 → 4.12\nRequires Minecraft 1.20.1\n"),
            "{summary}"
        );

        // Nothing known prints no line at all.
        plan.requirements = Some(PackRequirements::default());
        let summary = plan_summary(&fixture_profile(), &plan, false, true);
        assert!(!summary.contains("Requires"), "{summary}");
    }

    #[test]
    fn summary_pluralizes_single_file() {
        let mut plan = fixture_plan();
        plan.additions = (0..14)
            .map(|i| FileChange {
                rel_path: PathBuf::from(format!("mods/m{i}.jar")),
                kind: ChangeKind::Add,
                source: None,
                sha256: None,
            })
            .collect();
        plan.overlay_changes = vec![
            overlay_change("config/a.toml", OverlayChangeStatus::ReplacesChanged, true),
            overlay_change("config/b.toml", OverlayChangeStatus::ReplacesChanged, true),
            overlay_change("mods/x.jar", OverlayChangeStatus::Applied, true),
            overlay_change("mods/y.jar", OverlayChangeStatus::Applied, true),
        ];

        let summary = plan_summary(&fixture_profile(), &plan, false, true);

        assert!(summary.contains("  + 14 files\n"), "{summary}");
        assert!(summary.contains("  ~ 1 file\n"), "{summary}");
        assert!(summary.contains("  ✓ 2 files to apply"), "{summary}");
        assert!(
            summary.contains("  ⚠ 2 overlay files replace files that changed upstream"),
            "{summary}"
        );
    }

    #[test]
    fn first_update_block_points_at_adopt() {
        let block = first_update_block(
            FirstUpdateRisk {
                removals: 3,
                overwrites: 1,
            },
            "packctl adopt",
        );
        assert_eq!(
            block,
            "⚠ First update of an unmanaged server\n  3 files not in the pack would be deleted, \
             1 file that differs from the pack would be overwritten.\n  Adopt it first: packctl \
             adopt --as <version>\n\n"
        );
        let combined = format!("{block}{}", server_section(&[], &block));
        assert!(combined.contains("<version>\n\nServer\n"), "{combined}");
    }

    #[test]
    fn first_update_block_names_the_profile_typed() {
        let risk = FirstUpdateRisk {
            removals: 1,
            overwrites: 0,
        };
        let block = first_update_block(risk, &adopt_command(Some("atm10")));
        assert!(
            block.contains("Adopt it first: packctl adopt atm10 --as <version>"),
            "{block}"
        );
    }

    #[test]
    fn unmanaged_overwrites_get_their_own_block_and_json() {
        let mut plan = fixture_plan();
        plan.unmanaged_overwrites = vec![PathBuf::from("mods/changed.jar")];

        let summary = plan_summary(&fixture_profile(), &plan, false, true);
        assert!(
            summary.contains(
                "⚠ The pack would overwrite 1 file packctl never installed\n  mods/changed.jar\n"
            ),
            "{summary}"
        );
        assert!(summary.contains("--force-sync"), "{summary}");

        let json = plan_json("ATM10", &plan, None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["plan"]["unmanaged_overwrites"],
            serde_json::json!(["mods/changed.jar"])
        );
    }

    #[test]
    fn summary_has_single_blank_line_before_server() {
        let summary = plan_summary(&fixture_profile(), &fixture_plan(), false, true);
        let combined = format!("{summary}{}", server_section(&[], &summary));

        assert!(combined.contains("\n\nServer\n"), "{combined}");
        assert!(!combined.contains("\n\n\nServer"), "{combined}");
        assert!(combined.ends_with("Server\n  ✓ server checks passed\n"));

        let errors = vec!["controller is not usable".to_string()];
        let section = server_section(&errors, "no trailing blank\n");
        assert_eq!(section, "\nServer\n  ✗ controller is not usable\n");
    }

    #[test]
    fn overlay_in_sync_hidden_unless_verbose() {
        let mut plan = fixture_plan();
        plan.overlay_changes.push(overlay_change(
            "config/motd.conf",
            OverlayChangeStatus::Applied,
            false,
        ));

        let summary = plan_summary(&fixture_profile(), &plan, false, true);
        assert!(!summary.contains("config/motd.conf"), "{summary}");
        assert!(summary.contains("  ✓ 1 file to apply"), "{summary}");

        let summary = plan_summary(&fixture_profile(), &plan, true, true);
        assert!(
            summary.contains("  = config/motd.conf (overlay, in sync)"),
            "{summary}"
        );

        // A plan whose only overlay files are in place has nothing to do.
        let mut plan = empty_plan();
        plan.overlay_changes.push(overlay_change(
            "config/motd.conf",
            OverlayChangeStatus::Applied,
            false,
        ));
        assert!(plan.is_empty());
        let summary = plan_summary(&fixture_profile(), &plan, false, false);
        assert_eq!(summary, "No changes needed.\n");
        let summary = plan_summary(&fixture_profile(), &plan, true, false);
        assert!(
            summary.contains("  = config/motd.conf (overlay, in sync)"),
            "{summary}"
        );
    }

    #[test]
    fn plan_summary_verbose_lists_files_and_overlay_notes() {
        let summary = plan_summary(&fixture_profile(), &fixture_plan(), true, true);

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
        let summary = plan_summary(&fixture_profile(), &empty_plan(), false, false);

        assert!(summary.contains("No changes needed."));
        assert!(!summary.contains("Upstream"));
        assert!(!summary.contains("Overlay"));
    }

    #[test]
    fn verbose_only_notices_need_verbose() {
        let note = PlanNotice {
            verbose_only: true,
            ..PlanNotice::general("Loader version unknown".to_string())
        };
        for mut plan in [empty_plan(), fixture_plan()] {
            plan.notices.push(note.clone());
            let quiet = plan_summary(&fixture_profile(), &plan, false, false);
            assert!(!quiet.contains("Loader version unknown"), "{quiet}");
            let verbose = plan_summary(&fixture_profile(), &plan, true, false);
            assert!(verbose.contains("Loader version unknown"), "{verbose}");
        }
    }

    #[test]
    fn empty_plan_that_records_state_says_so_and_keeps_notices() {
        let mut plan = empty_plan();
        plan.notices.push(PlanNotice {
            path: Some(PathBuf::from("config/main.conf")),
            verbose_only: false,
            message: "Overlay conflict notice".to_string(),
        });

        let summary = plan_summary(&fixture_profile(), &plan, false, true);

        assert!(
            summary.starts_with(
                "No file changes needed; 'packctl update' would record 4.12 (id 12) as installed."
            ),
            "{summary}"
        );
        assert!(summary.contains("Overlay conflict notice"), "{summary}");
    }

    #[test]
    fn plan_summary_hides_zero_count_rows() {
        let mut plan = fixture_plan();
        plan.removals.clear();
        plan.overlay_changes = vec![overlay_change(
            "mods/grieflogger.jar",
            OverlayChangeStatus::Applied,
            true,
        )];

        let summary = plan_summary(&fixture_profile(), &plan, false, true);

        assert!(!summary.contains("  - "), "summary: {summary}");
        assert!(summary.contains("  ✓ 1 file to apply"));
        assert!(!summary.contains("changed upstream"), "summary: {summary}");
    }

    #[test]
    fn plan_summary_uses_none_when_no_from_version() {
        let mut plan = fixture_plan();
        plan.from_version = None;

        let summary = plan_summary(&fixture_profile(), &plan, false, true);

        assert!(summary.contains("none → 4.12"));
    }

    #[test]
    fn unmanaged_removals_get_their_own_block() {
        let mut plan = fixture_plan();
        plan.unmanaged_removals = vec![PathBuf::from("mods/upstream-old.jar")];

        let summary = plan_summary(&fixture_profile(), &plan, false, true);

        assert!(
            summary.contains(
                "⚠ 1 file packctl never installed would be deleted\n  mods/upstream-old.jar\n"
            ),
            "{summary}"
        );
        assert!(summary.contains("--force-sync"), "{summary}");
    }

    #[test]
    fn live_drift_gets_a_warning_with_a_copy_hint() {
        let mut plan = fixture_plan();
        plan.overlay_changes[0].live_drift = true;

        let summary = plan_summary(&fixture_profile(), &plan, false, true);

        assert!(
            summary.contains("⚠ The overlay would overwrite 1 live file changed outside packctl"),
            "{summary}"
        );
        assert!(summary.contains("  mods/grieflogger.jar\n"), "{summary}");
        let root = Path::new("/srv/atm10").join("mods/grieflogger.jar");
        let overlay = Path::new("/srv/overlay").join("mods/grieflogger.jar");
        assert!(
            summary.contains(&format!(
                "    cp -- {} {}\n",
                shell_quote(&root.display().to_string()),
                shell_quote(&overlay.display().to_string())
            )),
            "{summary}"
        );
    }

    #[test]
    fn live_drift_copy_hint_quotes_paths_for_the_shell() {
        let mut plan = fixture_plan();
        plan.overlay_changes[0].rel_path = PathBuf::from("config/it's $(id).toml");
        plan.overlay_changes[0].live_drift = true;

        let summary = plan_summary(&fixture_profile(), &plan, false, true);

        let hint = summary
            .lines()
            .find(|line| line.trim_start().starts_with("cp -- "))
            .unwrap_or_else(|| panic!("no cp hint: {summary}"));
        assert!(hint.contains(r"it'\''s $(id).toml'"), "{hint}");
        assert!(!hint.contains("it's"), "{hint}");
    }

    #[test]
    fn long_warning_lists_are_capped_unless_verbose() {
        let mut plan = fixture_plan();
        plan.unmanaged_removals = (0..12)
            .map(|i| PathBuf::from(format!("mods/m{i}.jar")))
            .collect();

        let summary = plan_summary(&fixture_profile(), &plan, false, true);
        assert!(summary.contains("  … and 2 more"), "{summary}");

        let summary = plan_summary(&fixture_profile(), &plan, true, true);
        assert!(!summary.contains("… and"), "{summary}");
        assert!(summary.contains("  mods/m11.jar\n"), "{summary}");
    }
    #[test]
    fn plan_json_has_profile_plan_and_risk() {
        let text = plan_json("ATM10", &fixture_plan(), None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["profile"], "ATM10");
        assert_eq!(value["plan"]["to_version"], "4.12");
        assert_eq!(
            value["plan"]["additions"][0]["rel_path"],
            "mods/upstream-a-new.jar"
        );
        assert!(value["first_update_risk"].is_null());

        let risky = plan_json(
            "ATM10",
            &fixture_plan(),
            Some(FirstUpdateRisk {
                removals: 2,
                overwrites: 1,
            }),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&risky).unwrap();
        assert_eq!(
            value["first_update_risk"],
            serde_json::json!({"removals": 2, "overwrites": 1})
        );
    }
}
