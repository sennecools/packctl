//! `packctl adopt` — record a hand-installed server as a pack version.
//!
//! The command classifies every file in the pack-owned folders against the
//! staged pack version and prints what adopting would do. Nothing changes
//! unless `--apply` is given; then the overlay copies, a managed
//! `.packctlignore` block and the state are written after a snapshot, so
//! `packctl rollback` undoes the adoption. The live pack folders are never
//! touched.

use std::path::Path;

use crate::cli::create::shell_quote;
use crate::config::profile::ServerProfile;
use crate::controllers::ServerStatus;
use crate::core::adopt::{
    AdoptAction, AdoptClass, AdoptEntry, AdoptOptions, AdoptReport, ApplyOptions,
    ExtraConfigPolicy, ModifiedPolicy, already_managed_error, apply_adoption,
};
use crate::core::detect::Detection;
use crate::core::updater::{PreparedAdoption, Updater};
use crate::error::{PackError, Result};
use crate::providers::{VersionSelector, sanitize};

/// Command-line switches for `packctl adopt`.
#[derive(Debug, Clone, Default)]
pub struct AdoptArgs {
    /// The version the server runs (`--as`).
    pub version: Option<String>,
    /// Guess the version from the jars in `mods/` (`--detect`).
    pub detect: bool,
    pub apply: bool,
    pub yes: bool,
    pub json: bool,
    pub verbose: bool,
    pub modified: ModifiedPolicy,
    pub extra_configs: ExtraConfigPolicy,
    pub keep_removed: bool,
    pub force: bool,
}

impl AdoptArgs {
    fn options(&self) -> AdoptOptions {
        AdoptOptions {
            modified: self.modified,
            extra_configs: self.extra_configs,
            keep_removed: self.keep_removed,
            ..AdoptOptions::default()
        }
    }
}

/// Runs `packctl adopt` for a server profile.
pub async fn run(server: Option<&str>, args: AdoptArgs) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;
    let prepared = prepare(&updater, &args).await?;
    print_report(&updater.profile, &prepared, &args);
    if !args.apply {
        if !args.json {
            outln!("Dry run: nothing was changed.");
            outln!(
                "{}",
                apply_hint(server, &prepared.adoption.report, &profile)
            );
        }
        return Ok(());
    }
    apply(&updater, &prepared, &args)
}

/// An adoption ready to be shown and applied.
pub(crate) struct PreparedRun {
    pub adoption: PreparedAdoption,
    /// True when the controller reports the server running.
    pub running: bool,
}

/// Steps before anything is printed: the state check, version selection,
/// classification and the sanity checks on the result.
pub(crate) async fn prepare(updater: &Updater, args: &AdoptArgs) -> Result<PreparedRun> {
    let profile = &updater.profile;
    let state = updater.load_state()?;
    if !state.is_empty() && !args.force {
        return Err(already_managed_error(&profile.name, &state));
    }

    let selector = match (&args.version, args.detect) {
        (Some(version), _) => VersionSelector::Query(version.clone()),
        (None, true) => match updater.detect_version().await? {
            Detection::Picked { version, score } => {
                note(
                    args.json,
                    &format!(
                        "Detected {} ({}% of the pack's mods match)",
                        sanitize(&version.name),
                        percent(score)
                    ),
                );
                VersionSelector::Id(version.id)
            }
            Detection::Ambiguous(candidates) => {
                let mut message =
                    "could not determine the version with confidence; pass --as <version>"
                        .to_string();
                if !candidates.is_empty() {
                    message.push_str("\nBest matches:");
                    for (version, score) in &candidates {
                        message.push_str(&format!(
                            "\n  {}  {}%  (id {})",
                            sanitize(&version.name),
                            percent(*score),
                            sanitize(&version.id)
                        ));
                    }
                }
                return Err(PackError::Config(message));
            }
        },
        (None, false) => {
            return Err(PackError::Config(
                "pass --as <version> or --detect".to_string(),
            ));
        }
    };

    let adoption = updater.prepare_adoption(&selector, &args.options()).await?;
    let report = &adoption.report;
    if report.missing_ratio > 0.5 && !args.force {
        return Err(PackError::Config(format!(
            "only {}% of the pack's files were found on this server; is '{}' the right version \
             and '{}' the right server root? Re-run with --force to adopt anyway.",
            percent(1.0 - report.missing_ratio),
            sanitize(&report.version.name),
            profile.server.root.display()
        )));
    }
    let running = matches!(updater.controller.status().await, Ok(ServerStatus::Running));
    if running {
        note(
            args.json,
            "note: the server is running; files may change while packctl reads them",
        );
    }
    Ok(PreparedRun { adoption, running })
}

/// Prints the report, as JSON with `--json`.
pub(crate) fn print_report(profile: &ServerProfile, prepared: &PreparedRun, args: &AdoptArgs) {
    let report = &prepared.adoption.report;
    if args.json {
        match serde_json::to_string_pretty(report) {
            Ok(json) => outln!("{json}"),
            Err(error) => errln!("error: could not serialize the report: {error}"),
        }
    } else {
        out!("{}", render_report(report, profile, args.verbose));
    }
}

/// Confirms (unless `--yes`) and applies a prepared adoption.
pub(crate) fn apply(updater: &Updater, prepared: &PreparedRun, args: &AdoptArgs) -> Result<()> {
    let profile = &updater.profile;
    if prepared.running && !args.force {
        return Err(PackError::Config(format!(
            "the server '{}' is running; stop it before 'packctl adopt --apply' so files do not \
             change while they are copied, or re-run with --force",
            profile.name
        )));
    }
    if !args.yes {
        crate::cli::prompt::require_tty("pass --yes to apply without confirmation")?;
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Apply adoption?")
            .default(false)
            .interact()
            .map_err(|err| {
                crate::cli::prompt::dialoguer_error("read adoption confirmation", err)
            })?;
        if !confirmed {
            outln!("Aborted.");
            return Ok(());
        }
    }

    let adoption = &prepared.adoption;
    let outcome = apply_adoption(
        &adoption.report,
        &adoption.prepared,
        profile,
        &adoption.overlay_files,
        &ApplyOptions { force: args.force },
    )?;
    let summary = applied_summary(
        profile,
        &adoption.report,
        outcome.overlay_written.len(),
        outcome.state.managed_files.len(),
    );
    if args.json {
        err!("{summary}");
    } else {
        out!("{summary}");
    }
    Ok(())
}

/// The report after `--apply` succeeded.
fn applied_summary(
    profile: &ServerProfile,
    report: &AdoptReport,
    overlay_files: usize,
    managed: usize,
) -> String {
    let mut lines = vec![
        format!(
            "Adopted '{}' as {}.",
            profile.name,
            sanitize(&report.version.name)
        ),
        format!(
            "  overlay:        {} copied to {}",
            files(overlay_files),
            profile.overlay.path.display()
        ),
        format!(
            "  .packctlignore: {} (block 'packctl adopt')",
            count(report.ignore_rules.len(), "rule", "rules")
        ),
        format!(
            "  state:          {} recorded",
            count(managed, "managed file", "managed files")
        ),
    ];
    if report.restore_count() > 0 {
        lines.push(format!(
            "The next update restores {} from the pack.",
            files(report.restore_count())
        ));
    }
    lines.push("Undo with: packctl rollback".to_string());
    lines.push("Next: packctl plan".to_string());
    lines.join("\n") + "\n"
}

/// How to run `packctl adopt` for the server the user named: with the
/// profile name, quoted for a POSIX shell, when one was given, so the hint
/// also works outside the server directory.
pub(crate) fn adopt_command(server: Option<&str>) -> String {
    match server {
        Some(name) => format!("packctl adopt {}", shell_quote(name)),
        None => "packctl adopt".to_string(),
    }
}

/// The dry-run hint naming the command that applies the adoption. The
/// version is given by id, quoted for a POSIX shell, so a provider-chosen
/// name can never turn into a command.
pub(crate) fn apply_hint(
    server: Option<&str>,
    report: &AdoptReport,
    profile: &ServerProfile,
) -> String {
    let adopt = adopt_command(server);
    let copies = report.overlay_copies().count();
    format!(
        "Apply with: {adopt} --as {} --apply\n  This writes {} ({}) to {}, {} to \
         .packctlignore and .packctl/state.json.",
        shell_quote(&report.version.id),
        files(copies),
        mib(report.overlay_bytes),
        profile.overlay.path.display(),
        count(report.ignore_rules.len(), "rule", "rules"),
    )
}

/// Prints a line that is not part of the report: to stderr under `--json`,
/// so stdout stays valid JSON.
fn note(json: bool, line: &str) {
    if json {
        errln!("{line}");
    } else {
        outln!("{line}");
    }
}

fn percent(score: f64) -> u32 {
    (score * 100.0).round().clamp(0.0, 100.0) as u32
}

fn count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural}")
    }
}

fn files(n: usize) -> String {
    count(n, "file", "files")
}

fn mib(bytes: u64) -> String {
    if bytes < 1024 * 1024 {
        return format!("{:.1} KiB", bytes as f64 / 1024.0);
    }
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Most paths listed per group without `--verbose`.
const MAX_LISTED: usize = 10;

/// One group of the report.
struct Group {
    label: &'static str,
    classes: &'static [AdoptClass],
    /// Restricts the group to entries with this action, when set.
    action: Option<AdoptAction>,
    /// Lists the paths without `--verbose`.
    list_by_default: bool,
}

const GROUPS: &[Group] = &[
    Group {
        label: "Matches the pack",
        classes: &[AdoptClass::Identical],
        action: None,
        list_by_default: false,
    },
    Group {
        label: "Reformatted only",
        classes: &[AdoptClass::CosmeticOnly],
        action: None,
        list_by_default: false,
    },
    Group {
        label: "Edited by you",
        classes: &[AdoptClass::Modified],
        action: None,
        list_by_default: true,
    },
    Group {
        label: "Added by you",
        classes: &[AdoptClass::Extra],
        action: None,
        list_by_default: true,
    },
    Group {
        label: "Replaced by you",
        classes: &[AdoptClass::Replaced],
        action: None,
        list_by_default: true,
    },
    Group {
        label: "Disabled by you",
        classes: &[AdoptClass::Disabled],
        action: None,
        list_by_default: true,
    },
    Group {
        label: "Removed by you",
        classes: &[AdoptClass::Missing],
        action: Some(AdoptAction::AddIgnoreRule),
        list_by_default: true,
    },
    Group {
        label: "Missing",
        classes: &[AdoptClass::Missing],
        action: Some(AdoptAction::RestoreOnNextUpdate),
        list_by_default: true,
    },
    Group {
        label: "Runtime data",
        classes: &[AdoptClass::RuntimeData],
        action: None,
        list_by_default: false,
    },
    Group {
        label: "Persistent (untouched)",
        classes: &[AdoptClass::Persistent],
        action: None,
        list_by_default: false,
    },
];

/// Where an entry ends up, as the report says it.
fn target(entry: &AdoptEntry, conflicts: &[std::path::PathBuf]) -> &'static str {
    match (entry.class, entry.action) {
        (AdoptClass::Persistent, _) => "",
        (AdoptClass::Replaced, AdoptAction::CopyToOverlay) => "overlay + ignore old",
        (_, AdoptAction::CopyToOverlay) => "overlay",
        (_, AdoptAction::KeepOverlay) if conflicts.contains(&entry.rel_path) => {
            "CONFLICT: overlay differs, update would overwrite"
        }
        (_, AdoptAction::KeepOverlay) => "already in overlay",
        (_, AdoptAction::AddIgnoreRule) => ".packctlignore",
        (AdoptClass::Identical, _) => "recorded",
        (_, AdoptAction::RecordManaged) => "pack version restored on next update",
        (_, AdoptAction::RecordManagedAcceptLocal) => "accepted as-is",
        (_, AdoptAction::RestoreOnNextUpdate) => "restored on next update",
        (AdoptClass::RuntimeData, AdoptAction::Skip) => "NOT protected",
        (_, AdoptAction::Skip) => "",
    }
}

/// The line for one entry, without its target.
fn entry_line(entry: &AdoptEntry) -> String {
    let path = sanitize(&entry.rel_path.to_string_lossy().replace('\\', "/"));
    let related = entry
        .related
        .as_ref()
        .map(|related| sanitize(&related.to_string_lossy().replace('\\', "/")))
        .unwrap_or_default();
    match entry.class {
        AdoptClass::Modified => format!("~ {path}"),
        AdoptClass::CosmeticOnly => format!("≈ {path}"),
        AdoptClass::Extra | AdoptClass::RuntimeData => format!("+ {path}"),
        AdoptClass::Replaced => format!("~ {related} → {path}"),
        AdoptClass::Disabled => format!("- {related} (disabled as {path})"),
        AdoptClass::Missing => format!("- {path}"),
        AdoptClass::Identical | AdoptClass::Persistent => format!("= {path}"),
    }
}

/// Renders the human-readable adoption report.
pub(crate) fn render_report(
    report: &AdoptReport,
    profile: &ServerProfile,
    verbose: bool,
) -> String {
    let mut lines = vec![
        profile.name.clone(),
        format!(
            "Adopting as {} (id {})",
            sanitize(&report.version.name),
            sanitize(&report.version.id)
        ),
    ];
    let mut scanned: Vec<String> = report
        .scanned_dirs
        .iter()
        .map(|dir| format!("{}/", sanitize(dir)))
        .collect();
    if scanned.is_empty() {
        scanned.push("no pack folders".to_string());
    }
    let mut scanned_line = format!("Scanned: {}", scanned.join(", "));
    if !report.scanned_top_level.is_empty() {
        scanned_line.push_str(&format!(
            " + {}",
            count(
                report.scanned_top_level.len(),
                "top-level file",
                "top-level files"
            )
        ));
    }
    lines.push(scanned_line);
    lines.push(String::new());

    for group in GROUPS {
        let entries: Vec<&AdoptEntry> = report
            .entries
            .iter()
            .filter(|entry| group.classes.contains(&entry.class))
            .filter(|entry| group.action.is_none_or(|action| entry.action == action))
            .collect();
        if entries.is_empty() {
            continue;
        }
        let targets: Vec<&str> = entries
            .iter()
            .map(|entry| target(entry, &report.overlay_conflicts))
            .collect();
        let shared = targets
            .iter()
            .all(|t| *t == targets[0])
            .then_some(targets[0]);
        let unit = if entries.len() == 1 { "file " } else { "files" };
        let mut header = format!("{:<22} {:>5} {unit}", group.label, entries.len());
        if let Some(shared) = shared.filter(|t| !t.is_empty()) {
            header.push_str(&format!("  → {shared}"));
        }
        lines.push(header.trim_end().to_string());

        if !(verbose || group.list_by_default) {
            continue;
        }
        // Entries ignored through one directory rule are shown once.
        let mut shown: Vec<String> = Vec::new();
        for (entry, entry_target) in entries.iter().zip(&targets) {
            let collapsed = entry.action == AdoptAction::AddIgnoreRule
                && entry.ignore_rule.as_deref().is_some_and(|rule| {
                    rule != entry.rel_path.to_string_lossy().replace('\\', "/")
                });
            let text = if collapsed && !verbose {
                format!(
                    "+ {}/",
                    sanitize(entry.ignore_rule.as_deref().unwrap_or_default())
                )
            } else {
                entry_line(entry)
            };
            let line = match shared {
                Some(_) => format!("  {text}"),
                None if entry_target.is_empty() => format!("  {text}"),
                None => format!("  {text}  → {entry_target}"),
            };
            if !shown.contains(&line) {
                shown.push(line);
            }
        }
        let limit = if verbose { shown.len() } else { MAX_LISTED };
        let hidden = shown.len().saturating_sub(limit);
        lines.extend(shown.into_iter().take(limit));
        if hidden > 0 {
            lines.push(format!("  … and {hidden} more (use -v)"));
        }
    }
    lines.push(format!(
        "{:<22} {:>5}",
        "Overlay conflicts",
        report.overlay_conflicts.len()
    ));
    for path in report
        .overlay_conflicts
        .iter()
        .take(if verbose { usize::MAX } else { MAX_LISTED })
    {
        lines.push(format!(
            "  ! {}  (the overlay holds a different file; the next update would overwrite the \
             live file, so it needs --force-sync until you reconcile them)",
            sanitize(&path.to_string_lossy().replace('\\', "/"))
        ));
    }
    if report.overlay_conflicts.len() > MAX_LISTED && !verbose {
        lines.push(format!(
            "  … and {} more (use -v)",
            report.overlay_conflicts.len() - MAX_LISTED
        ));
    }

    let restore = report.restore_count();
    if restore > 0 {
        lines.push(String::new());
        lines.push(format!(
            "The next update will restore {} from the pack.",
            files(restore)
        ));
    }
    if !is_within_root(profile) && report.overlay_copies().next().is_some() {
        lines.push(String::new());
        lines.push(format!(
            "The overlay '{}' is outside the server root: 'packctl rollback' cannot remove the \
             files adopt copies there, so --apply needs --force.",
            profile.overlay.path.display()
        ));
    }
    if !report.warnings.is_empty() {
        lines.push(String::new());
        lines.extend(
            report
                .warnings
                .iter()
                .map(|warning| format!("warning: {}", sanitize(warning))),
        );
    }
    lines.push(String::new());
    lines.join("\n") + "\n"
}

fn is_within_root(profile: &ServerProfile) -> bool {
    crate::fs::paths::is_within(&profile.server.root, &profile.overlay.path)
}

/// True when `<root>/mods` holds at least one jar: the server already has a
/// pack installed, so `create` offers to adopt it.
pub(crate) fn has_installed_mods(root: &Path) -> bool {
    std::fs::read_dir(root.join("mods")).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| crate::fs::paths::is_jar_path(&entry.path()))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::core::state::{InstalledState, ManagedFile, StateStore};
    use crate::providers::PackVersion;
    use crate::testutil::{FakeController, FakeProvider, test_profile, write_files};

    fn entry(path: &str, class: AdoptClass, action: AdoptAction) -> AdoptEntry {
        AdoptEntry {
            rel_path: PathBuf::from(path),
            class,
            action,
            pack: None,
            live: Some(ManagedFile::new("abc", 10)),
            related: None,
            ignore_rule: None,
            related_rule: None,
        }
    }

    fn sample_report() -> AdoptReport {
        let mut entries = Vec::new();
        for index in 0..3 {
            entries.push(entry(
                &format!("mods/pack{index}.jar"),
                AdoptClass::Identical,
                AdoptAction::RecordManaged,
            ));
        }
        entries.push(entry(
            "config/fmt.toml",
            AdoptClass::CosmeticOnly,
            AdoptAction::RecordManagedAcceptLocal,
        ));
        for index in 0..12 {
            entries.push(entry(
                &format!("config/edited{index:02}.toml"),
                AdoptClass::Modified,
                AdoptAction::CopyToOverlay,
            ));
        }
        entries.push(entry(
            "mods/grieflogger-1.2.jar",
            AdoptClass::Extra,
            AdoptAction::CopyToOverlay,
        ));
        for name in ["luckperms.conf", "libs/x.jar"] {
            entries.push(AdoptEntry {
                ignore_rule: Some("config/luckperms".to_string()),
                ..entry(
                    &format!("config/luckperms/{name}"),
                    AdoptClass::Extra,
                    AdoptAction::AddIgnoreRule,
                )
            });
        }
        entries.push(AdoptEntry {
            related: Some(PathBuf::from("mods/jei-15.2.0.jar")),
            related_rule: Some("mods/jei-15.2.0.jar".to_string()),
            ..entry(
                "mods/jei-15.3.0.jar",
                AdoptClass::Replaced,
                AdoptAction::CopyToOverlay,
            )
        });
        entries.push(AdoptEntry {
            related: Some(PathBuf::from("mods/laggy.jar")),
            ignore_rule: Some("mods/laggy.jar.disabled".to_string()),
            ..entry(
                "mods/laggy.jar.disabled",
                AdoptClass::Disabled,
                AdoptAction::AddIgnoreRule,
            )
        });
        entries.push(AdoptEntry {
            live: None,
            ..entry(
                "mods/removed.jar",
                AdoptClass::Missing,
                AdoptAction::AddIgnoreRule,
            )
        });
        entries.push(AdoptEntry {
            live: None,
            ..entry(
                "config/gone.toml",
                AdoptClass::Missing,
                AdoptAction::RestoreOnNextUpdate,
            )
        });
        entries.push(entry(
            "config/cache.db",
            AdoptClass::RuntimeData,
            AdoptAction::AddIgnoreRule,
        ));
        entries.push(entry(
            "server.properties",
            AdoptClass::Persistent,
            AdoptAction::Skip,
        ));
        AdoptReport {
            profile: "atm10".to_string(),
            version: PackVersion {
                id: "5125810".to_string(),
                name: "All the Mods 10-2.31".to_string(),
                ..Default::default()
            },
            scanned_dirs: vec![
                "config".to_string(),
                "kubejs".to_string(),
                "mods".to_string(),
            ],
            scanned_top_level: vec!["a.txt".to_string(), "b.txt".to_string()],
            entries,
            ignore_rules: vec![
                "config/cache.db".to_string(),
                "config/luckperms".to_string(),
                "mods/jei-15.2.0.jar".to_string(),
            ],
            overlay_conflicts: vec![PathBuf::from("config/clash.toml")],
            warnings: vec!["mods/a.jar differs from the pack's jar with the same name".to_string()],
            overlay_bytes: 3 * 1024 * 1024,
            missing_ratio: 0.1,
            unprotected: Vec::new(),
        }
    }

    fn profile() -> ServerProfile {
        let mut profile = test_profile(
            &PathBuf::from("/srv/atm10"),
            &PathBuf::from("/srv/atm10/overlay"),
        );
        profile.name = "atm10".to_string();
        profile
    }

    #[test]
    fn render_report_groups_and_truncation() {
        let report = sample_report();
        let text = render_report(&report, &profile(), false);
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines[0], "atm10");
        assert_eq!(lines[1], "Adopting as All the Mods 10-2.31 (id 5125810)");
        assert_eq!(
            lines[2],
            "Scanned: config/, kubejs/, mods/ + 2 top-level files"
        );
        assert!(
            text.contains("Matches the pack           3 files  → recorded\n"),
            "{text}"
        );
        assert!(text.contains("Reformatted only           1 file   → accepted as-is\n"));
        assert!(text.contains("Edited by you             12 files  → overlay\n"));
        assert!(text.contains("  ~ config/edited00.toml\n"));
        assert!(text.contains("  ~ config/edited09.toml\n"));
        assert!(!text.contains("config/edited10.toml"), "truncated at 10");
        assert!(text.contains("  … and 2 more (use -v)\n"));
        // Mixed targets: the header has no arrow, each line has one, and a
        // directory rule is shown once.
        assert!(
            text.contains("Added by you               3 files\n"),
            "{text}"
        );
        assert!(text.contains("  + mods/grieflogger-1.2.jar  → overlay\n"));
        assert_eq!(
            text.matches("  + config/luckperms/  → .packctlignore\n")
                .count(),
            1,
            "{text}"
        );
        assert!(text.contains("Replaced by you            1 file   → overlay + ignore old\n"));
        assert!(text.contains("  ~ mods/jei-15.2.0.jar → mods/jei-15.3.0.jar\n"));
        assert!(text.contains("Disabled by you            1 file   → .packctlignore\n"));
        assert!(text.contains("  - mods/laggy.jar (disabled as mods/laggy.jar.disabled)\n"));
        assert!(text.contains("Removed by you             1 file   → .packctlignore\n"));
        assert!(text.contains("Missing                    1 file   → restored on next update\n"));
        assert!(text.contains("Runtime data               1 file   → .packctlignore\n"));
        assert!(text.contains("Persistent (untouched)     1 file\n"));
        assert!(text.contains("Overlay conflicts          1\n"));
        assert!(text.contains("  ! config/clash.toml"));
        assert!(text.contains("The next update will restore 1 file from the pack.\n"));
        assert!(
            text.contains("warning: mods/a.jar differs from the pack's jar with the same name\n")
        );
        // Identical files are only listed with --verbose.
        assert!(!text.contains("mods/pack0.jar"));

        let verbose = render_report(&report, &profile(), true);
        assert!(verbose.contains("  = mods/pack0.jar\n"));
        assert!(verbose.contains("  ~ config/edited11.toml\n"));
        assert!(verbose.contains("  + config/luckperms/libs/x.jar  → .packctlignore\n"));
        assert!(!verbose.contains("use -v"));

        // Group order follows the report.
        let position = |needle: &str| text.find(needle).unwrap();
        assert!(position("Matches the pack") < position("Edited by you"));
        assert!(position("Edited by you") < position("Added by you"));
        assert!(position("Removed by you") < position("Missing "));
        assert!(position("Runtime data") < position("Overlay conflicts"));
    }

    #[test]
    fn empty_groups_are_omitted() {
        let mut report = sample_report();
        report
            .entries
            .retain(|entry| entry.class == AdoptClass::Identical);
        report.overlay_conflicts.clear();
        report.warnings.clear();
        let text = render_report(&report, &profile(), false);
        assert!(!text.contains("Edited by you"));
        assert!(!text.contains("warning:"));
        assert!(text.contains("Overlay conflicts          0\n"));
    }

    #[test]
    fn report_json_round_trip() {
        let report = sample_report();
        let json = serde_json::to_string_pretty(&report).unwrap();
        let back: AdoptReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["entries"][0]["class"], "Identical");
        assert_eq!(value["entries"][0]["action"], "RecordManaged");
        assert_eq!(value["version"]["id"], "5125810");
    }

    #[test]
    fn adopt_command_names_and_quotes_the_profile() {
        assert_eq!(adopt_command(None), "packctl adopt");
        assert_eq!(adopt_command(Some("atm10")), "packctl adopt atm10");
        assert_eq!(
            adopt_command(Some("my server")),
            "packctl adopt 'my server'"
        );
    }

    #[test]
    fn apply_hint_names_the_version_by_quoted_id() {
        let mut report = sample_report();
        let hint = apply_hint(None, &report, &profile());
        assert!(
            hint.starts_with("Apply with: packctl adopt --as 5125810 --apply\n"),
            "{hint}"
        );
        assert!(
            hint.contains("This writes 14 files (3.0 MiB) to /srv/atm10/overlay, 3 rules"),
            "{hint}"
        );

        report.version.id = "pack 1.0 $(rm -rf ~).zip".to_string();
        let hint = apply_hint(Some("my server"), &report, &profile());
        assert!(
            hint.starts_with(
                "Apply with: packctl adopt 'my server' --as 'pack 1.0 $(rm -rf ~).zip' --apply"
            ),
            "{hint}"
        );
    }

    fn server_with_mods(tmp: &tempfile::TempDir) -> PathBuf {
        let root = tmp.path().join("server");
        write_files(
            &root,
            &[
                ("mods/upstream.jar", b"UP"),
                ("config/upstream.toml", b"TOML"),
                ("mods/added.jar", b"mine"),
            ],
        );
        root
    }

    fn args(version: &str) -> AdoptArgs {
        AdoptArgs {
            version: Some(version.to_string()),
            ..AdoptArgs::default()
        }
    }

    #[tokio::test]
    async fn refuses_non_empty_state_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let root = server_with_mods(&tmp);
        StateStore::at(&root)
            .unwrap()
            .save(&InstalledState {
                installed_version: Some("1.0".to_string()),
                ..InstalledState::default()
            })
            .unwrap();
        let updater = Updater::new(
            test_profile(&root, &root.join("overlay")),
            Box::new(FakeProvider::single()),
            Box::new(FakeController::stopped()),
        );

        let error = prepare(&updater, &args("2.0")).await.err().unwrap();
        assert_eq!(
            error.to_string(),
            "'test-server' already has packctl state (installed 1.0); adopt is for servers \
             packctl has never managed. Re-run with --force to replace it."
        );

        let forced = AdoptArgs {
            force: true,
            ..args("2.0")
        };
        let prepared = prepare(&updater, &forced).await.unwrap();
        assert_eq!(prepared.adoption.report.version.id, "v2");
    }

    #[tokio::test]
    async fn apply_refuses_a_running_server_and_applies_a_stopped_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = server_with_mods(&tmp);
        let running = Updater::new(
            test_profile(&root, &root.join("overlay")),
            Box::new(FakeProvider::single()),
            Box::new(FakeController::new()),
        );
        let apply_args = AdoptArgs {
            apply: true,
            yes: true,
            ..args("2.0")
        };
        let prepared = prepare(&running, &apply_args).await.unwrap();
        assert!(prepared.running);
        let error = apply(&running, &prepared, &apply_args).unwrap_err();
        assert!(error.to_string().contains("is running"), "{error}");
        assert!(!root.join(".packctl/state.json").exists());

        let stopped = Updater::new(
            test_profile(&root, &root.join("overlay")),
            Box::new(FakeProvider::single()),
            Box::new(FakeController::stopped()),
        );
        let prepared = prepare(&stopped, &apply_args).await.unwrap();
        apply(&stopped, &prepared, &apply_args).unwrap();
        let state = StateStore::at(&root).unwrap().load().unwrap();
        assert_eq!(state.installed_version.as_deref(), Some("2.0"));
        assert_eq!(
            std::fs::read(root.join("overlay/mods/added.jar")).unwrap(),
            b"mine"
        );
    }

    #[tokio::test]
    async fn mostly_missing_pack_needs_force() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write_files(&root, &[("mods/other.jar", b"x")]);
        let updater = Updater::new(
            test_profile(&root, &root.join("overlay")),
            Box::new(FakeProvider::single()),
            Box::new(FakeController::stopped()),
        );
        let error = prepare(&updater, &args("2.0")).await.err().unwrap();
        assert!(
            error
                .to_string()
                .starts_with("only 0% of the pack's files were found on this server"),
            "{error}"
        );
    }

    #[test]
    fn installed_mods_are_detected() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!has_installed_mods(tmp.path()));
        write_files(tmp.path(), &[("mods/readme.txt", b"x")]);
        assert!(!has_installed_mods(tmp.path()));
        write_files(tmp.path(), &[("mods/a.jar", b"x")]);
        assert!(has_installed_mods(tmp.path()));
    }
}
