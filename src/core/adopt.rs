//! Adopting a hand-installed server.
//!
//! `packctl adopt` records a server that already runs a pack version as that
//! version, without downloading it again and without wiping what the admin
//! changed. Every file in the pack-owned folders is classified against the
//! staged pack:
//!
//! - files equal to the pack are recorded as managed;
//! - files that were only reformatted are recorded with their local hash as
//!   an accepted variant;
//! - edited files and added mods go to the overlay, so they win over every
//!   later update;
//! - added configs, runtime data and files the admin removed or disabled get
//!   `.packctlignore` rules, so the next update leaves them alone.
//!
//! [`classify_server`] only reads. [`apply_adoption`] writes the overlay
//! copies, the managed `.packctlignore` block and the state, after taking a
//! snapshot that `packctl rollback` restores. The live pack folders are never
//! modified.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::profile::ServerProfile;
use crate::core::configdiff::cosmetic_equal;
use crate::core::ignore::{IgnoreRules, has_managed_block, parse_rules, rule_for_exact_path};
use crate::core::modid::mod_identity;
use crate::core::overlay::OverlayFile;
use crate::core::ownership::FilePolicy;
use crate::core::planner::{
    collect_live_files, live_matches_hash, pack_owned_dirs, pack_top_level_files,
};
use crate::core::reserved::{IGNORE_FILE, ReservedPaths};
use crate::core::snapshot::{Snapshot, create_snapshot, reject_symlink_components};
use crate::core::state::{
    AdoptionRecord, InstalledState, ManagedFile, StateStore, write_atomically,
};
use crate::core::updater::{StateSource, target_minecraft};
use crate::error::{PackError, Result};
use crate::fs::copy::{copy_file, remove_file, remove_tree};
use crate::fs::hashing::{hash_files_parallel, sha256_file};
use crate::fs::paths::{is_jar_path, is_within, safe_join};
use crate::providers::{PackVersion, PreparedFile, PreparedPack, sanitize};

/// Relative path of the state file inside the server root.
const STATE_REL_PATH: &str = ".packctl/state.json";

/// Largest text file read for the cosmetic comparison.
const MAX_COSMETIC_READ: u64 = 256 * 1024;

/// How a file on the server relates to the pack version being adopted.
///
/// The order is the order of the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AdoptClass {
    /// The live file equals the pack's.
    Identical,
    /// The live file differs from the pack's only in comments, blank lines
    /// or trailing whitespace.
    CosmeticOnly,
    /// The live file was edited.
    Modified,
    /// The live file is not in the pack (added by the admin).
    Extra,
    /// A pack mod jar was swapped for another jar of the same mod.
    Replaced,
    /// A pack mod jar was disabled (`mods/X.jar` renamed `mods/X.jar.disabled`).
    Disabled,
    /// The pack ships the file but the server lacks it.
    Missing,
    /// Data a mod writes at runtime (databases, logs, locks, large files).
    RuntimeData,
    /// The pack ships persistent server data (`world/`, `server.properties`
    /// ...); packctl never manages it.
    Persistent,
}

/// What adopting does with a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AdoptAction {
    /// Record the pack fingerprint as managed.
    RecordManaged,
    /// Record the pack fingerprint and accept the live file as a variant.
    RecordManagedAcceptLocal,
    /// Copy the live file into the overlay.
    CopyToOverlay,
    /// Add a `.packctlignore` rule.
    AddIgnoreRule,
    /// Nothing now; the next update writes the pack's file.
    RestoreOnNextUpdate,
    /// The overlay already has this path; it is left as it is.
    KeepOverlay,
    /// Nothing at all.
    Skip,
}

/// One classified file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptEntry {
    /// Path relative to the server root (forward slashes).
    pub rel_path: PathBuf,
    pub class: AdoptClass,
    pub action: AdoptAction,
    /// The pack's fingerprint, when the pack ships the path.
    pub pack: Option<ManagedFile>,
    /// The live file's fingerprint, when it exists. `sha256` is empty when
    /// the file was not hashed (runtime data and files only ignored).
    pub live: Option<ManagedFile>,
    /// The pack path a replaced or disabled jar stands for.
    pub related: Option<PathBuf>,
    /// The `.packctlignore` rule covering `rel_path` (a directory rule when
    /// a whole directory is ignored), for `AddIgnoreRule`.
    pub ignore_rule: Option<String>,
    /// The rule that keeps `related` out of updates, for replaced and
    /// disabled jars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_rule: Option<String>,
}

/// What to do with pack files the admin edited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ModifiedPolicy {
    /// Copy the edited file into the overlay (it wins over every update).
    #[default]
    Overlay,
    /// Record the pack's version; the next update writes it back.
    Upstream,
    /// Ignore the file; updates never touch it again.
    Ignore,
}

/// What to do with config files the admin added.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ExtraConfigPolicy {
    /// Ignore them, so the mods that own them keep updating them.
    #[default]
    Ignore,
    /// Freeze them in the overlay.
    Overlay,
}

/// Classification options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptOptions {
    pub modified: ModifiedPolicy,
    pub extra_configs: ExtraConfigPolicy,
    /// Ignore pack files the server lacks instead of restoring them.
    pub keep_removed: bool,
    /// Added files bigger than this are runtime data (jars excepted).
    pub large_file_bytes: u64,
    /// Files hashed at once.
    pub hash_concurrency: usize,
}

impl Default for AdoptOptions {
    fn default() -> Self {
        AdoptOptions {
            modified: ModifiedPolicy::default(),
            extra_configs: ExtraConfigPolicy::default(),
            keep_removed: false,
            large_file_bytes: 16 * 1024 * 1024,
            hash_concurrency: 8,
        }
    }
}

/// The result of classifying a server against a pack version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdoptReport {
    pub profile: String,
    pub version: PackVersion,
    /// The pack-owned folders that were scanned (`config`, `mods`, ...).
    pub scanned_dirs: Vec<String>,
    /// The pack's top-level files that were looked at.
    pub scanned_top_level: Vec<String>,
    /// Every classified file, sorted by class and path.
    pub entries: Vec<AdoptEntry>,
    /// The `.packctlignore` rules adopt writes, sorted and deduplicated.
    pub ignore_rules: Vec<String>,
    /// Overlay paths that already hold a different file; they are kept.
    pub overlay_conflicts: Vec<PathBuf>,
    pub warnings: Vec<String>,
    /// Bytes copied into the overlay.
    pub overlay_bytes: u64,
    /// The fraction of pack files the server lacks.
    pub missing_ratio: f64,
    /// Runtime data no `.packctlignore` rule can protect; the next update
    /// would delete it, so applying refuses unless forced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unprotected: Vec<PathBuf>,
}

impl AdoptReport {
    /// The entries of one class.
    pub fn of_class(&self, class: AdoptClass) -> impl Iterator<Item = &AdoptEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.class == class)
    }

    /// The files adopt copies into the overlay.
    pub fn overlay_copies(&self) -> impl Iterator<Item = &AdoptEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.action == AdoptAction::CopyToOverlay)
    }

    /// How many files the next update writes back from the pack.
    pub fn restore_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.action == AdoptAction::RestoreOnNextUpdate)
            .count()
    }
}

/// What [`classify_server`] looks at.
pub struct AdoptContext<'a> {
    pub profile_name: &'a str,
    pub prepared: &'a PreparedPack,
    /// The overlay as it is now.
    pub overlay: &'a [OverlayFile],
    /// The `.packctlignore` rules as they are now.
    pub ignore: &'a IgnoreRules,
    pub reserved: &'a ReservedPaths,
    pub policy: &'a FilePolicy,
    pub server_root: &'a Path,
    pub overlay_root: &'a Path,
}

/// Classifies the live server against the prepared pack version. Read-only.
///
/// The live set is what an update would sweep: every regular file under the
/// pack-owned folders plus the pack's top-level files, without persistent,
/// reserved, already ignored or symlinked paths. Live files are only hashed
/// where needed: a size that differs from the pack's means "edited" without
/// hashing, and runtime data is never hashed.
pub fn classify_server(ctx: &AdoptContext<'_>, opts: &AdoptOptions) -> Result<AdoptReport> {
    let root = ctx.server_root;
    let prepared = ctx.prepared;
    let mut entries: Vec<AdoptEntry> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // The pack files adopt reasons about.
    let mut pack: BTreeMap<String, &PreparedFile> = BTreeMap::new();
    for file in &prepared.files {
        let rel = file.rel_path.as_path();
        if ctx.reserved.is_reserved(rel) || ctx.ignore.is_ignored(rel) {
            continue;
        }
        let key = rel_key(rel);
        if ctx.policy.is_persistent(rel) {
            entries.push(AdoptEntry {
                pack: Some(fingerprint(file)),
                ..entry(&key, AdoptClass::Persistent, AdoptAction::Skip)
            });
            continue;
        }
        pack.insert(key, file);
    }
    let all_pack_keys: Vec<String> = prepared
        .files
        .iter()
        .map(|f| rel_key(&f.rel_path))
        .collect();

    // The live files an update would consider.
    let scanned_dirs = pack_owned_dirs(prepared, ctx.reserved);
    let mut live_paths = Vec::new();
    for dir in &scanned_dirs {
        collect_live_files(
            root,
            Path::new(dir),
            ctx.policy,
            ctx.ignore,
            ctx.reserved,
            &mut live_paths,
        )?;
    }
    let mut scanned_top_level = Vec::new();
    for rel in pack_top_level_files(prepared) {
        if ctx.reserved.is_reserved(&rel)
            || ctx.ignore.is_ignored(&rel)
            || ctx.policy.is_persistent(&rel)
        {
            continue;
        }
        scanned_top_level.push(rel_key(&rel));
        live_paths.push(rel);
    }
    let mut live: BTreeMap<String, u64> = BTreeMap::new();
    for rel in live_paths {
        if ctx.policy.is_persistent(&rel) || ctx.reserved.is_reserved(&rel) {
            continue;
        }
        let path = safe_join(root, &rel)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                live.insert(rel_key(&rel), metadata.len());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(PackError::io(format!("stat '{}'", path.display()), error));
            }
        }
    }

    // Hash the shared files whose sizes agree.
    let same_size: Vec<String> = pack
        .iter()
        .filter(|(key, file)| live.get(*key) == Some(&file.size))
        .map(|(key, _)| key.clone())
        .collect();
    let mut live_hashes = hash_live(root, &same_size, opts.hash_concurrency)?;

    // Files on both sides.
    let mut pack_only: BTreeSet<String> = BTreeSet::new();
    for (key, file) in &pack {
        let Some(&size) = live.get(key) else {
            pack_only.insert(key.clone());
            continue;
        };
        let live_print = ManagedFile::new(live_hashes.get(key).cloned().unwrap_or_default(), size);
        let base = AdoptEntry {
            pack: Some(fingerprint(file)),
            live: Some(live_print),
            ..entry(key, AdoptClass::Identical, AdoptAction::RecordManaged)
        };
        if size == file.size && live_hashes.get(key) == Some(&file.sha256) {
            entries.push(base);
        } else if is_cosmetic(
            &prepared.root.join(&file.rel_path),
            &safe_join(root, Path::new(key))?,
            size,
            file.size,
        )? {
            entries.push(AdoptEntry {
                class: AdoptClass::CosmeticOnly,
                action: AdoptAction::RecordManagedAcceptLocal,
                ..base
            });
        } else {
            let action = match opts.modified {
                ModifiedPolicy::Overlay => AdoptAction::CopyToOverlay,
                ModifiedPolicy::Upstream => AdoptAction::RecordManaged,
                ModifiedPolicy::Ignore => AdoptAction::AddIgnoreRule,
            };
            if is_jar_path(Path::new(key)) {
                warnings.push(format!(
                    "{} differs from the pack's jar with the same name",
                    sanitize(key)
                ));
            }
            entries.push(AdoptEntry {
                class: AdoptClass::Modified,
                action,
                ..base
            });
        }
    }
    let mut live_only: BTreeSet<String> = live
        .keys()
        .filter(|key| !pack.contains_key(*key))
        .cloned()
        .collect();

    // Disabled jars: mods/X.disabled where the pack ships mods/X and the
    // server lacks it.
    for key in live_only.clone() {
        let Some(target) = strip_suffix_ignore_case(&key, ".disabled") else {
            continue;
        };
        if !key.starts_with("mods/") || !pack_only.contains(target) {
            continue;
        }
        let target = target.to_string();
        pack_only.remove(&target);
        live_only.remove(&key);
        entries.push(AdoptEntry {
            pack: pack.get(&target).map(|file| fingerprint(file)),
            live: Some(ManagedFile::new(String::new(), live[&key])),
            related: Some(PathBuf::from(&target)),
            related_rule: rule_for_exact_path(&target),
            ..entry(&key, AdoptClass::Disabled, AdoptAction::AddIgnoreRule)
        });
    }

    // Replaced jars: a pack-only and a live-only jar in mods/ that hold the
    // same mod.
    let pack_jars: Vec<String> = pack_only
        .iter()
        .filter(|key| is_top_level_mod_jar(key))
        .cloned()
        .collect();
    let live_jars: Vec<String> = live_only
        .iter()
        .filter(|key| is_top_level_mod_jar(key))
        .cloned()
        .collect();
    if !pack_jars.is_empty() && !live_jars.is_empty() {
        let live_ids: Vec<(String, Vec<String>)> = live_jars
            .iter()
            .map(|key| Ok((key.clone(), mod_identity(&safe_join(root, Path::new(key))?))))
            .collect::<Result<_>>()?;
        let mut paired: HashSet<String> = HashSet::new();
        for pack_key in pack_jars {
            let ids = mod_identity(&prepared.root.join(&pack[&pack_key].rel_path));
            if ids.is_empty() {
                continue;
            }
            let Some((live_key, _)) = live_ids.iter().find(|(live_key, live_ids)| {
                !paired.contains(live_key) && live_ids.iter().any(|id| ids.contains(id))
            }) else {
                continue;
            };
            paired.insert(live_key.clone());
            pack_only.remove(&pack_key);
            live_only.remove(live_key);
            entries.push(AdoptEntry {
                pack: Some(fingerprint(pack[&pack_key])),
                live: Some(ManagedFile::new(String::new(), live[live_key])),
                related: Some(PathBuf::from(&pack_key)),
                related_rule: rule_for_exact_path(&pack_key),
                ..entry(live_key, AdoptClass::Replaced, AdoptAction::CopyToOverlay)
            });
        }
    }

    // Everything else on the server.
    for key in &live_only {
        let size = live[key];
        // Added mods and scripts are frozen in the overlay; added configs
        // stay mutable (ignored) unless asked otherwise.
        let to_overlay = ["mods/", "kubejs/", "scripts/", "defaultconfigs/"]
            .iter()
            .any(|dir| key.starts_with(dir))
            || (key.starts_with("config/") && opts.extra_configs == ExtraConfigPolicy::Overlay);
        let (class, action) = if is_runtime_data(key, size, opts.large_file_bytes) {
            (AdoptClass::RuntimeData, AdoptAction::AddIgnoreRule)
        } else if to_overlay {
            (AdoptClass::Extra, AdoptAction::CopyToOverlay)
        } else {
            (AdoptClass::Extra, AdoptAction::AddIgnoreRule)
        };
        entries.push(AdoptEntry {
            live: Some(ManagedFile::new(String::new(), size)),
            ..entry(key, class, action)
        });
    }

    // Everything else in the pack.
    for key in &pack_only {
        let action = if key.starts_with("mods/") || opts.keep_removed {
            AdoptAction::AddIgnoreRule
        } else {
            AdoptAction::RestoreOnNextUpdate
        };
        entries.push(AdoptEntry {
            pack: Some(fingerprint(pack[key])),
            ..entry(key, AdoptClass::Missing, action)
        });
    }

    // One rule for directories that only hold ignored additions.
    let live_keys: BTreeSet<&str> = live.keys().map(String::as_str).collect();
    let collapsed = collapse_directories(&entries, &live_keys, &all_pack_keys);
    // Rules, with fallbacks where a path cannot be written as a rule.
    let mut unprotected = Vec::new();
    for entry in &mut entries {
        if let (Some(related), None) = (&entry.related, &entry.related_rule) {
            warnings.push(format!(
                "cannot express an ignore rule for '{}'; the next update will restore it",
                sanitize(&rel_key(related))
            ));
        }
        if entry.action != AdoptAction::AddIgnoreRule {
            continue;
        }
        let key = rel_key(&entry.rel_path);
        if let Some(dir) = collapsed.iter().find(|dir| is_under(&key, dir)) {
            entry.ignore_rule = Some(dir.clone());
            continue;
        }
        if let Some(rule) = rule_for_exact_path(&key) {
            entry.ignore_rule = Some(rule);
            continue;
        }
        if let Some(rule) = ancestor_rule(&key, &all_pack_keys) {
            entry.ignore_rule = Some(rule);
            continue;
        }
        let shown = sanitize(&key);
        match entry.class {
            AdoptClass::RuntimeData => {
                entry.action = AdoptAction::Skip;
                unprotected.push(entry.rel_path.clone());
                warnings.push(format!(
                    "cannot express an ignore rule for '{shown}'; it will be removed by the next \
                     update unless you protect it manually"
                ));
            }
            AdoptClass::Missing => {
                entry.action = AdoptAction::RestoreOnNextUpdate;
                warnings.push(format!(
                    "cannot express an ignore rule for '{shown}'; the next update will restore it"
                ));
            }
            _ => {
                entry.action = AdoptAction::CopyToOverlay;
                warnings.push(format!(
                    "cannot express an ignore rule for '{shown}'; it is kept in the overlay instead"
                ));
            }
        }
    }

    // Hash what is copied or accepted as a variant.
    let unhashed: Vec<String> = entries
        .iter()
        .filter(|entry| {
            matches!(entry.action, AdoptAction::CopyToOverlay)
                || entry.class == AdoptClass::CosmeticOnly
        })
        .map(|entry| rel_key(&entry.rel_path))
        .filter(|key| !live_hashes.contains_key(key))
        .collect();
    live_hashes.extend(hash_live(root, &unhashed, opts.hash_concurrency)?);
    for entry in &mut entries {
        if let Some(live) = &mut entry.live
            && let Some(hash) = live_hashes.get(&rel_key(&entry.rel_path))
        {
            live.sha256 = hash.clone();
        }
    }

    // Never overwrite an overlay file that already exists.
    let mut overlay_conflicts = Vec::new();
    for entry in &mut entries {
        if entry.action != AdoptAction::CopyToOverlay {
            continue;
        }
        let target = safe_join(ctx.overlay_root, &entry.rel_path)?;
        let same = match std::fs::symlink_metadata(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(PackError::io(format!("stat '{}'", target.display()), error));
            }
            Ok(metadata) if metadata.is_file() => {
                let live = entry.live.as_ref().map(|live| live.sha256.as_str());
                Some(sha256_file(&target)?.as_str()) == live
            }
            Ok(_) => false,
        };
        entry.action = AdoptAction::KeepOverlay;
        if !same {
            warnings.push(format!(
                "the overlay already holds a different '{}': the next update would overwrite \
                 your live file with it, so it refuses without --force-sync. Copy the live file \
                 into the overlay (or delete the overlay file) to reconcile them",
                sanitize(&rel_key(&entry.rel_path))
            ));
            overlay_conflicts.push(entry.rel_path.clone());
        }
    }

    // An overlay file the live file differs from was never applied; the
    // overlay wins, so the next update writes it over the live file.
    let overlay_by_key: HashMap<String, &OverlayFile> = ctx
        .overlay
        .iter()
        .map(|file| (rel_key(&file.rel_path), file))
        .collect();
    for entry in &entries {
        if entry.action == AdoptAction::KeepOverlay {
            continue;
        }
        let key = rel_key(&entry.rel_path);
        if let (Some(overlay_file), Some(live)) = (overlay_by_key.get(&key), &entry.live)
            && live_differs_from_overlay(live, overlay_file)
        {
            warnings.push(format!(
                "the overlay holds a different '{}'; the next update writes it over the live file",
                sanitize(&key)
            ));
        }
    }
    // The same for overlay files outside the scanned folders.
    let classified: HashSet<String> = entries
        .iter()
        .map(|entry| rel_key(&entry.rel_path))
        .collect();
    for file in ctx.overlay {
        let key = rel_key(&file.rel_path);
        if classified.contains(&key) {
            continue;
        }
        let live = safe_join(root, &file.rel_path)?;
        let live_is_file =
            std::fs::symlink_metadata(&live).is_ok_and(|metadata| metadata.is_file());
        if live_is_file && !live_matches_hash(root, &file.rel_path, file.size, &file.sha256)? {
            warnings.push(format!(
                "the overlay holds a different '{}'; the next update writes it over the live file",
                sanitize(&key)
            ));
        }
    }

    entries.sort_by(|a, b| {
        a.class
            .cmp(&b.class)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    let ignore_rules: Vec<String> = entries
        .iter()
        .flat_map(|entry| {
            let own = (entry.action == AdoptAction::AddIgnoreRule)
                .then(|| entry.ignore_rule.clone())
                .flatten();
            own.into_iter().chain(entry.related_rule.clone())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let overlay_bytes = entries
        .iter()
        .filter(|entry| entry.action == AdoptAction::CopyToOverlay)
        .filter_map(|entry| entry.live.as_ref().map(|live| live.size))
        .sum();
    let missing = entries
        .iter()
        .filter(|entry| entry.class == AdoptClass::Missing)
        .count();
    let missing_ratio = if pack.is_empty() {
        0.0
    } else {
        missing as f64 / pack.len() as f64
    };
    overlay_conflicts.sort();
    unprotected.sort();
    warnings.sort();
    warnings.dedup();

    Ok(AdoptReport {
        profile: ctx.profile_name.to_string(),
        version: prepared.version.clone(),
        scanned_dirs,
        scanned_top_level,
        entries,
        ignore_rules,
        overlay_conflicts,
        warnings,
        overlay_bytes,
        missing_ratio,
        unprotected,
    })
}

fn entry(key: &str, class: AdoptClass, action: AdoptAction) -> AdoptEntry {
    AdoptEntry {
        rel_path: PathBuf::from(key),
        class,
        action,
        pack: None,
        live: None,
        related: None,
        ignore_rule: None,
        related_rule: None,
    }
}

fn fingerprint(file: &PreparedFile) -> ManagedFile {
    ManagedFile::new(file.sha256.clone(), file.size)
}

fn rel_key(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// True when `key` is `dir` itself or lies beneath it.
fn is_under(key: &str, dir: &str) -> bool {
    key == dir
        || key
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn strip_suffix_ignore_case<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let split = text.len().checked_sub(suffix.len())?;
    (text.is_char_boundary(split) && text[split..].eq_ignore_ascii_case(suffix))
        .then(|| &text[..split])
        .filter(|rest| !rest.is_empty())
}

/// True for `mods/<name>.jar` directly inside `mods/`.
fn is_top_level_mod_jar(key: &str) -> bool {
    key.strip_prefix("mods/")
        .is_some_and(|name| !name.contains('/') && is_jar_path(Path::new(name)))
}

/// File extensions of data mods write while the server runs.
const RUNTIME_EXTENSIONS: &[&str] = &[
    "db", "sqlite", "h2", "lock", "log", "bak", "old", "tmp", "dat",
];

/// True for runtime data: databases, locks, logs, backups, and large files
/// (jars excepted: a big jar is still a mod).
fn is_runtime_data(key: &str, size: u64, large_file_bytes: u64) -> bool {
    let lower = key.to_ascii_lowercase();
    let ext = Path::new(&lower)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    RUNTIME_EXTENSIONS.contains(&ext)
        || lower.ends_with(".mv.db")
        || (size > large_file_bytes && ext != "jar")
}

/// True when the live file differs from the pack's only cosmetically.
fn is_cosmetic(pack_path: &Path, live_path: &Path, live_size: u64, pack_size: u64) -> Result<bool> {
    let ext = live_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_string();
    if live_size > MAX_COSMETIC_READ || pack_size > MAX_COSMETIC_READ {
        return Ok(false);
    }
    if !crate::core::configdiff::supports(&ext) {
        return Ok(false);
    }
    let read = |path: &Path| {
        std::fs::read(path)
            .map_err(|error| PackError::io(format!("read '{}'", path.display()), error))
    };
    Ok(cosmetic_equal(&read(pack_path)?, &read(live_path)?, &ext))
}

/// SHA-256 of live files, keyed by relative path.
fn hash_live(root: &Path, keys: &[String], concurrency: usize) -> Result<HashMap<String, String>> {
    let paths: Vec<PathBuf> = keys
        .iter()
        .map(|key| safe_join(root, Path::new(key)))
        .collect::<Result<_>>()?;
    let hashes = hash_files_parallel(&paths, concurrency)?;
    Ok(keys
        .iter()
        .cloned()
        .zip(hashes.into_iter().map(|(_, hash)| hash))
        .collect())
}

/// Directories (at least two levels deep, like `config/luckperms`) whose
/// every live file is an ignored addition or runtime data and that hold no
/// pack file: each becomes one rule. Shallowest directories win.
fn collapse_directories(
    entries: &[AdoptEntry],
    live_keys: &BTreeSet<&str>,
    pack_keys: &[String],
) -> Vec<String> {
    let eligible: HashSet<String> = entries
        .iter()
        .filter(|entry| {
            matches!(entry.class, AdoptClass::Extra | AdoptClass::RuntimeData)
                && entry.action == AdoptAction::AddIgnoreRule
        })
        .map(|entry| rel_key(&entry.rel_path))
        .collect();
    let mut candidates: BTreeSet<(usize, String)> = BTreeSet::new();
    for key in &eligible {
        let parts: Vec<&str> = key.split('/').collect();
        for depth in 2..parts.len() {
            candidates.insert((depth, parts[..depth].join("/")));
        }
    }
    let mut chosen: Vec<String> = Vec::new();
    for (_, dir) in candidates {
        if chosen.iter().any(|done| is_under(&dir, done)) {
            continue;
        }
        let only_eligible = live_keys
            .iter()
            .filter(|key| is_under(key, &dir))
            .all(|key| eligible.contains(*key));
        let has_pack_file = pack_keys.iter().any(|key| is_under(key, &dir));
        if only_eligible
            && !has_pack_file
            && rule_for_exact_path(&dir).as_deref() == Some(dir.as_str())
        {
            chosen.push(dir);
        }
    }
    chosen
}

/// The rule for the nearest ancestor directory of `key` (at least two levels
/// deep) that holds no pack file, for a path no exact rule can express.
fn ancestor_rule(key: &str, pack_keys: &[String]) -> Option<String> {
    let parts: Vec<&str> = key.split('/').collect();
    (2..parts.len()).rev().find_map(|depth| {
        let dir = parts[..depth].join("/");
        if pack_keys.iter().any(|pack| is_under(pack, &dir)) {
            return None;
        }
        rule_for_exact_path(&dir)
    })
}

/// Options for [`apply_adoption`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Replace existing packctl state, adopt even though some runtime data
    /// cannot be protected, and write an overlay outside the server root.
    pub force: bool,
}

/// What [`apply_adoption`] did.
#[derive(Debug)]
pub struct AdoptOutcome {
    /// Overlay files written (absolute paths).
    pub overlay_written: Vec<PathBuf>,
    /// Rules in the managed `.packctlignore` block.
    pub ignore_rules_added: usize,
    /// The snapshot `packctl rollback` restores to undo the adoption.
    pub snapshot: Snapshot,
    /// The state that was saved.
    pub state: InstalledState,
}

/// Writes an adoption: overlay copies, the managed `.packctlignore` block
/// and the state, under the server lock and after a snapshot.
///
/// The snapshot holds `state.json` and `.packctlignore` (when they exist)
/// and tracks the overlay files adopt creates inside the server root, so
/// `packctl rollback` removes exactly those and restores the rest. Overlay
/// files that already existed are never tracked or touched.
///
/// Every overlay copy is hashed after copying and must equal what
/// classification saw; a file that changed meanwhile (a running server
/// writing it) aborts the adoption. On any failure the files written so far
/// are removed again and the previous `.packctlignore` is put back, so a
/// failed adoption changes nothing.
pub fn apply_adoption(
    report: &AdoptReport,
    prepared: &PreparedPack,
    profile: &ServerProfile,
    overlay: &[OverlayFile],
    opts: &ApplyOptions,
) -> Result<AdoptOutcome> {
    let root = &profile.server.root;
    let overlay_root = &profile.overlay.path;
    let copies: Vec<&AdoptEntry> = report.overlay_copies().collect();

    if !report.unprotected.is_empty() && !opts.force {
        let listed: Vec<String> = report
            .unprotected
            .iter()
            .map(|path| format!("  {}", sanitize(&rel_key(path))))
            .collect();
        return Err(PackError::Config(format!(
            "no .packctlignore rule can protect these runtime files, so the next update would \
             delete them:\n{}\nRename or move them, or re-run with --force to adopt anyway.",
            listed.join("\n")
        )));
    }
    let overlay_inside = is_within(root, overlay_root);
    if !overlay_inside && !copies.is_empty() && !opts.force {
        return Err(PackError::Config(format!(
            "the overlay '{}' is outside the server root, so 'packctl rollback' could not \
             remove the files adopt copies there; re-run with --force to adopt anyway",
            overlay_root.display()
        )));
    }

    let store = StateStore::at(root)?;
    let _lock = store.lock()?;
    // Checked again under the lock: an update may have committed since the
    // report was prepared.
    let previous = store.load()?;
    if !previous.is_empty() && !opts.force {
        return Err(already_managed_error(&profile.name, &previous));
    }

    // Every copy target must still be free, and no symlink may redirect it.
    let mut targets = Vec::with_capacity(copies.len());
    for entry in &copies {
        let dest = safe_join(overlay_root, &entry.rel_path)?;
        reject_symlink_components(overlay_root, &dest)?;
        match std::fs::symlink_metadata(&dest) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(PackError::io(format!("stat '{}'", dest.display()), error)),
            Ok(_) => {
                return Err(PackError::Config(format!(
                    "overlay file '{}' appeared after the adoption was planned; run 'packctl \
                     adopt' again",
                    dest.display()
                )));
            }
        }
        targets.push(dest);
    }

    // (a) Snapshot.
    let state_path = root.join(STATE_REL_PATH);
    let ignore_path = root.join(IGNORE_FILE);
    let files: Vec<&Path> = [state_path.as_path(), ignore_path.as_path()]
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    let mut tracked: Vec<String> = vec![STATE_REL_PATH.to_string(), IGNORE_FILE.to_string()];
    if overlay_inside {
        for dest in &targets {
            if let Ok(rel) = dest.strip_prefix(root) {
                tracked.push(rel_key(rel));
            }
        }
    }
    let tracked_refs: Vec<&str> = tracked.iter().map(String::as_str).collect();
    let snapshot = create_snapshot(root, &files, &tracked_refs)?;

    let original_ignore = match std::fs::read_to_string(&ignore_path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(undo(
                &snapshot,
                &[],
                None,
                &ignore_path,
                PackError::io(format!("read '{}'", ignore_path.display()), error),
            ));
        }
    };

    // (b) Overlay copies, each verified against the classified hash.
    let mut written: Vec<PathBuf> = Vec::new();
    for (entry, dest) in copies.iter().zip(&targets) {
        if let Err(error) = copy_verified(root, entry, dest, &mut written) {
            return Err(undo(
                &snapshot,
                &written,
                original_ignore.as_deref(),
                &ignore_path,
                error,
            ));
        }
    }

    // (c) The managed .packctlignore block.
    let new_ignore = match write_ignore_block(report, original_ignore.as_deref(), &ignore_path) {
        Ok(rules) => rules,
        Err(error) => {
            return Err(undo(
                &snapshot,
                &written,
                original_ignore.as_deref(),
                &ignore_path,
                error,
            ));
        }
    };

    // (d) The state.
    let state = match adopted_state(report, prepared, profile, overlay, &new_ignore, &copies) {
        Ok(state) => state,
        Err(error) => {
            return Err(undo(
                &snapshot,
                &written,
                original_ignore.as_deref(),
                &ignore_path,
                error,
            ));
        }
    };
    if let Err(error) = store.save(&state) {
        return Err(undo(
            &snapshot,
            &written,
            original_ignore.as_deref(),
            &ignore_path,
            error,
        ));
    }

    Ok(AdoptOutcome {
        overlay_written: written,
        ignore_rules_added: report.ignore_rules.len(),
        snapshot,
        state,
    })
}

/// The error for adopting a server packctl already manages.
pub fn already_managed_error(name: &str, state: &InstalledState) -> PackError {
    PackError::Config(format!(
        "'{name}' already has packctl state (installed {}); adopt is for servers packctl has \
         never managed. Re-run with --force to replace it.",
        state.installed_version.as_deref().unwrap_or("no version")
    ))
}

/// Copies one live file into the overlay and checks the copy.
fn copy_verified(
    root: &Path,
    entry: &AdoptEntry,
    dest: &Path,
    written: &mut Vec<PathBuf>,
) -> Result<()> {
    let source = safe_join(root, &entry.rel_path)?;
    reject_symlink_components(root, &source)?;
    let is_file = std::fs::symlink_metadata(&source).is_ok_and(|metadata| metadata.is_file());
    if !is_file {
        return Err(changed_while_adopting(&entry.rel_path));
    }
    // Recorded before copying: a copy that fails halfway (a full disk, a
    // read error) leaves a truncated file behind, and undo must remove it.
    written.push(dest.to_path_buf());
    copy_file(&source, dest)?;
    let expected = entry.live.as_ref().map(|live| live.sha256.as_str());
    if Some(sha256_file(dest)?.as_str()) != expected {
        return Err(changed_while_adopting(&entry.rel_path));
    }
    Ok(())
}

/// True when a classified live file is known to differ from an overlay file
/// (by size, or by hash when it was hashed).
fn live_differs_from_overlay(live: &ManagedFile, overlay: &OverlayFile) -> bool {
    live.size != overlay.size || (!live.sha256.is_empty() && live.sha256 != overlay.sha256)
}

fn changed_while_adopting(rel: &Path) -> PackError {
    PackError::Config(format!(
        "'{}' changed while adopting; stop the server or retry",
        rel.display()
    ))
}

/// Upserts the managed block into `.packctlignore` (only when there are
/// rules or an older block to replace) and returns the rules now in force.
fn write_ignore_block(
    report: &AdoptReport,
    original: Option<&str>,
    path: &Path,
) -> Result<IgnoreRules> {
    let existing = original.unwrap_or_default();
    if report.ignore_rules.is_empty() && !has_managed_block(existing) {
        return parse_rules(existing);
    }
    let comment = format!(
        "{} {} (id {})",
        Utc::now().format("%Y-%m-%d"),
        report.version.name,
        report.version.id
    );
    let content =
        crate::core::ignore::upsert_managed_block(existing, &comment, &report.ignore_rules)?;
    let rules = parse_rules(&content)?;
    write_atomically(path, content.as_bytes(), "ignore file")?;
    Ok(rules)
}

/// The state an adoption records.
fn adopted_state(
    report: &AdoptReport,
    prepared: &PreparedPack,
    profile: &ServerProfile,
    overlay: &[OverlayFile],
    ignore: &IgnoreRules,
    copies: &[&AdoptEntry],
) -> Result<InstalledState> {
    // The overlay after adoption: what was there plus the new copies.
    let mut overlay_after: HashMap<String, ManagedFile> = overlay
        .iter()
        .map(|file| {
            (
                rel_key(&file.rel_path),
                ManagedFile::new(file.sha256.clone(), file.size),
            )
        })
        .collect();
    // The overlay files recorded as applied:
    // - the new copies and every existing overlay file the live file equals
    //   (checked on disk, also outside the scanned folders), so an
    //   immediate `plan` has nothing to record;
    // - an overlay conflict (a kept overlay file the admin's live edit
    //   differs from), so the next update reports live drift and refuses
    //   to overwrite that edit without --force-sync.
    // Any other overlay file the live file differs from (prepared but never
    // applied) is left out, so the next update applies it instead of
    // reporting live drift.
    let root = &profile.server.root;
    let mut overlay_applied: HashMap<String, ManagedFile> = HashMap::new();
    for file in overlay {
        let recorded = report.overlay_conflicts.contains(&file.rel_path)
            || live_matches_hash(root, &file.rel_path, file.size, &file.sha256)?;
        if recorded {
            overlay_applied.insert(
                rel_key(&file.rel_path),
                ManagedFile::new(file.sha256.clone(), file.size),
            );
        }
    }
    for entry in copies {
        if let Some(live) = &entry.live {
            let record = ManagedFile::new(live.sha256.clone(), live.size);
            overlay_after.insert(rel_key(&entry.rel_path), record.clone());
            overlay_applied.insert(rel_key(&entry.rel_path), record);
        }
    }

    let reserved = ReservedPaths::for_profile(profile);
    let policy = FilePolicy::default_policy();
    let mut managed_files = HashMap::new();
    for entry in &report.entries {
        let key = rel_key(&entry.rel_path);
        let Some(pack) = &entry.pack else {
            continue;
        };
        if overlay_after.contains_key(&key) || ignore.is_ignored(&entry.rel_path) {
            continue;
        }
        let record = match (entry.class, entry.action) {
            (AdoptClass::Identical, _) => pack.clone(),
            (AdoptClass::CosmeticOnly, _) => match &entry.live {
                Some(live) => pack.clone().with_local(live.sha256.clone()),
                None => pack.clone(),
            },
            (AdoptClass::Modified, AdoptAction::RecordManaged) => pack.clone(),
            _ => continue,
        };
        managed_files.insert(key, record);
    }
    let overlay_shadowed = prepared
        .files
        .iter()
        .filter(|file| {
            let rel = file.rel_path.as_path();
            !policy.is_persistent(rel) && !reserved.is_reserved(rel) && !ignore.is_ignored(rel)
        })
        .filter(|file| overlay_after.contains_key(&rel_key(&file.rel_path)))
        .map(|file| (rel_key(&file.rel_path), fingerprint(file)))
        .collect();

    let source = StateSource::for_profile(profile);
    Ok(InstalledState {
        installed_version: Some(report.version.name.clone()),
        provider_version_id: Some(report.version.id.clone()),
        provider: Some(source.provider),
        project: source.project,
        minecraft: target_minecraft(prepared),
        managed_files,
        overlay_shadowed,
        overlay_applied,
        last_successful_update: None,
        adopted: Some(AdoptionRecord {
            at: Utc::now(),
            version: report.version.name.clone(),
            provider_version_id: report.version.id.clone(),
            overlay_added: copies
                .iter()
                .map(|entry| rel_key(&entry.rel_path))
                .collect(),
            ignore_rules: report.ignore_rules.clone(),
        }),
    })
}

/// Undoes a failed adoption: removes the overlay files written, puts the
/// previous `.packctlignore` back, and deletes the snapshot (nothing it
/// recorded changed any more). When the cleanup itself fails, the snapshot
/// is kept and named.
fn undo(
    snapshot: &Snapshot,
    written: &[PathBuf],
    original_ignore: Option<&str>,
    ignore_path: &Path,
    cause: PackError,
) -> PackError {
    let mut problems = Vec::new();
    for path in written.iter().rev() {
        if let Err(error) = remove_file(path) {
            problems.push(error.to_string());
        }
    }
    let current = std::fs::read_to_string(ignore_path).ok();
    if current.as_deref() != original_ignore {
        let restored = match original_ignore {
            Some(content) => write_atomically(ignore_path, content.as_bytes(), "ignore file"),
            None => remove_file(ignore_path),
        };
        if let Err(error) = restored {
            problems.push(error.to_string());
        }
    }
    if problems.is_empty() {
        let _ = remove_tree(&snapshot.dir);
        PackError::Other(format!(
            "{cause}\nThe adoption was not applied; nothing was changed."
        ))
    } else {
        PackError::Other(format!(
            "{cause}\nThe adoption was not applied, but cleaning up failed:\n  {}\nRestore with \
             'packctl rollback' (snapshot {}).",
            problems.join("\n  "),
            snapshot.dir.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ignore::parse_rules;
    use crate::core::modid::write_mod_jar;
    use crate::core::overlay::OverlayEngine;
    use crate::core::snapshot::{list_snapshots, mark_restored, restore_snapshot, rollback_target};
    use crate::fs::hashing::sha256_bytes;
    use crate::testutil::{test_profile, tree_snapshot, write_files};

    /// A staged pack version next to a live server root.
    struct Fixture {
        tmp: tempfile::TempDir,
        root: PathBuf,
        prepared: PreparedPack,
        profile: ServerProfile,
    }

    impl Fixture {
        fn new(pack: &[(&str, &[u8])], live: &[(&str, &[u8])]) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("server");
            let staged = tmp.path().join("staging/server");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&staged).unwrap();
            write_files(&staged, pack);
            write_files(&root, live);
            let files = pack
                .iter()
                .map(|(rel, data)| PreparedFile {
                    rel_path: PathBuf::from(rel),
                    size: data.len() as u64,
                    sha256: sha256_bytes(data),
                })
                .collect();
            let prepared = PreparedPack {
                name: "fixture-pack".to_string(),
                version: PackVersion {
                    id: "id-1".to_string(),
                    name: "1.0".to_string(),
                    ..Default::default()
                },
                root: staged,
                files,
                requirements: None,
                notices: Vec::new(),
            };
            let profile = test_profile(&root, &root.join("overlay"));
            Fixture {
                tmp,
                root,
                prepared,
                profile,
            }
        }

        /// Adds a file to the staged pack after construction (for jars
        /// built with the zip crate).
        fn stage(&mut self, rel: &str, source: &Path) {
            let dest = self.prepared.root.join(rel);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(source, &dest).unwrap();
            let data = std::fs::read(&dest).unwrap();
            self.prepared.files.push(PreparedFile {
                rel_path: PathBuf::from(rel),
                size: data.len() as u64,
                sha256: sha256_bytes(&data),
            });
        }

        fn overlay_root(&self) -> PathBuf {
            self.profile.overlay.path.clone()
        }

        fn overlay(&self) -> Vec<OverlayFile> {
            OverlayEngine::new(self.overlay_root()).scan().unwrap()
        }

        fn classify(&self, opts: &AdoptOptions) -> AdoptReport {
            let (ignore, _) =
                crate::core::updater::load_ignore_rules_for_adoption(&self.root).unwrap();
            let overlay = self.overlay();
            let reserved = ReservedPaths::for_profile(&self.profile);
            let policy = FilePolicy::default_policy();
            classify_server(
                &AdoptContext {
                    profile_name: &self.profile.name,
                    prepared: &self.prepared,
                    overlay: &overlay,
                    ignore: &ignore,
                    reserved: &reserved,
                    policy: &policy,
                    server_root: &self.root,
                    overlay_root: &self.overlay_root(),
                },
                opts,
            )
            .unwrap()
        }

        /// An updater whose provider offers this fixture's pack version.
        fn updater(&self) -> crate::core::updater::Updater {
            let files = self
                .prepared
                .files
                .iter()
                .map(|file| {
                    let key = rel_key(&file.rel_path);
                    let data = std::fs::read(self.prepared.root.join(&file.rel_path)).unwrap();
                    (key, data)
                })
                .collect();
            let provider = crate::testutil::FakeProvider {
                versions: vec![(
                    self.prepared.version.id.clone(),
                    self.prepared.version.name.clone(),
                    files,
                )],
            };
            crate::core::updater::Updater::new(
                self.profile.clone(),
                Box::new(provider),
                Box::new(crate::testutil::FakeController::stopped()),
            )
        }

        fn apply(&self, report: &AdoptReport) -> Result<AdoptOutcome> {
            apply_adoption(
                report,
                &self.prepared,
                &self.profile,
                &self.overlay(),
                &ApplyOptions::default(),
            )
        }
    }

    fn defaults() -> AdoptOptions {
        AdoptOptions::default()
    }

    fn find<'a>(report: &'a AdoptReport, path: &str) -> &'a AdoptEntry {
        report
            .entries
            .iter()
            .find(|entry| entry.rel_path == Path::new(path))
            .unwrap_or_else(|| panic!("no entry for {path}: {:#?}", report.entries))
    }

    fn has_entry(report: &AdoptReport, path: &str) -> bool {
        report
            .entries
            .iter()
            .any(|entry| entry.rel_path == Path::new(path))
    }

    #[test]
    fn failed_overlay_copy_is_still_recorded_for_undo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write_files(&root, &[("config/y.toml", b"edited")]);
        // Something in the way makes the copy fail after it was attempted.
        let dest = tmp.path().join("overlay/config/y.toml");
        std::fs::create_dir_all(&dest).unwrap();
        let entry = AdoptEntry {
            rel_path: PathBuf::from("config/y.toml"),
            class: AdoptClass::Modified,
            action: AdoptAction::CopyToOverlay,
            pack: None,
            live: Some(ManagedFile::new(sha256_bytes(b"edited"), 6)),
            related: None,
            ignore_rule: None,
            related_rule: None,
        };

        let mut written = Vec::new();
        assert!(copy_verified(&root, &entry, &dest, &mut written).is_err());
        assert_eq!(written, vec![dest]);
    }

    #[test]
    fn identical_files_recorded_managed() {
        let files: &[(&str, &[u8])] = &[
            ("mods/a.jar", b"jar-a"),
            ("config/a.toml", b"a = 1\n"),
            ("user_jvm_args.txt", b"-Xmx8G\n"),
        ];
        let fx = Fixture::new(files, files);
        let report = fx.classify(&defaults());

        for (path, data) in files {
            let entry = find(&report, path);
            assert_eq!(entry.class, AdoptClass::Identical, "{path}");
            assert_eq!(entry.action, AdoptAction::RecordManaged, "{path}");
            assert_eq!(entry.live.as_ref().unwrap().sha256, sha256_bytes(data));
        }
        assert_eq!(report.scanned_dirs, vec!["config", "mods"]);
        assert_eq!(report.scanned_top_level, vec!["user_jvm_args.txt"]);
        assert!(report.ignore_rules.is_empty());
        assert_eq!(report.overlay_bytes, 0);
        assert_eq!(report.missing_ratio, 0.0);
    }

    #[test]
    fn cosmetic_toml_reformat_is_accepted_not_overlaid() {
        let live: &[u8] = b"[general]\r\nlimit = 4   \r\n\r\n# my note\r\n";
        let fx = Fixture::new(
            &[("config/a.toml", b"# Pack default\n[general]\nlimit = 4\n")],
            &[("config/a.toml", live)],
        );
        let report = fx.classify(&defaults());
        let entry = find(&report, "config/a.toml");
        assert_eq!(entry.class, AdoptClass::CosmeticOnly);
        assert_eq!(entry.action, AdoptAction::RecordManagedAcceptLocal);
        assert_eq!(entry.live.as_ref().unwrap().sha256, sha256_bytes(live));
        assert_eq!(report.overlay_copies().count(), 0);
    }

    #[test]
    fn modified_config_copied_to_overlay() {
        let fx = Fixture::new(
            &[("config/ftbquests/quests/data.snbt", b"{a:1}")],
            &[("config/ftbquests/quests/data.snbt", b"{a:2,b:3}")],
        );
        let report = fx.classify(&defaults());
        let entry = find(&report, "config/ftbquests/quests/data.snbt");
        assert_eq!(entry.class, AdoptClass::Modified);
        assert_eq!(entry.action, AdoptAction::CopyToOverlay);
        assert_eq!(
            entry.live.as_ref().unwrap().sha256,
            sha256_bytes(b"{a:2,b:3}"),
            "a copied file is hashed even when its size differs"
        );
        assert_eq!(report.overlay_bytes, 9);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn modified_jar_with_the_pack_name_warns() {
        let fx = Fixture::new(&[("mods/a.jar", b"pack")], &[("mods/a.jar", b"patched")]);
        let report = fx.classify(&defaults());
        assert_eq!(
            find(&report, "mods/a.jar").action,
            AdoptAction::CopyToOverlay
        );
        assert_eq!(
            report.warnings,
            vec!["mods/a.jar differs from the pack's jar with the same name"]
        );
    }

    #[test]
    fn modified_policy_upstream_and_ignore() {
        let pack: &[(&str, &[u8])] = &[("config/a.toml", b"a = 1\n")];
        let live: &[(&str, &[u8])] = &[("config/a.toml", b"a = 2\n")];
        let fx = Fixture::new(pack, live);

        let upstream = fx.classify(&AdoptOptions {
            modified: ModifiedPolicy::Upstream,
            ..defaults()
        });
        let entry = find(&upstream, "config/a.toml");
        assert_eq!(
            (entry.class, entry.action),
            (AdoptClass::Modified, AdoptAction::RecordManaged)
        );
        assert!(upstream.ignore_rules.is_empty());

        let ignore = fx.classify(&AdoptOptions {
            modified: ModifiedPolicy::Ignore,
            ..defaults()
        });
        let entry = find(&ignore, "config/a.toml");
        assert_eq!(entry.action, AdoptAction::AddIgnoreRule);
        assert_eq!(entry.ignore_rule.as_deref(), Some("config/a.toml"));
        assert_eq!(ignore.ignore_rules, vec!["config/a.toml"]);
    }

    #[test]
    fn extra_mod_jar_goes_to_overlay() {
        let fx = Fixture::new(
            &[("mods/a.jar", b"a")],
            &[
                ("mods/a.jar", b"a"),
                ("mods/grieflogger-1.2.jar", b"griefs"),
            ],
        );
        let report = fx.classify(&defaults());
        let entry = find(&report, "mods/grieflogger-1.2.jar");
        assert_eq!(
            (entry.class, entry.action),
            (AdoptClass::Extra, AdoptAction::CopyToOverlay)
        );
        assert_eq!(report.overlay_bytes, 6);

        // kubejs/ and defaultconfigs/ additions go there too.
        let fx = Fixture::new(
            &[("kubejs/server.js", b"x"), ("defaultconfigs/a.toml", b"a")],
            &[
                ("kubejs/server.js", b"x"),
                ("kubejs/mine.js", b"mine"),
                ("defaultconfigs/a.toml", b"a"),
                ("defaultconfigs/b.toml", b"b"),
            ],
        );
        let report = fx.classify(&defaults());
        assert_eq!(
            find(&report, "kubejs/mine.js").action,
            AdoptAction::CopyToOverlay
        );
        assert_eq!(
            find(&report, "defaultconfigs/b.toml").action,
            AdoptAction::CopyToOverlay
        );
    }

    #[test]
    fn extra_config_gets_ignore_rule() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a")],
            &[("config/a.toml", b"a"), ("config/b.toml", b"b")],
        );
        let report = fx.classify(&defaults());
        let entry = find(&report, "config/b.toml");
        assert_eq!(
            (entry.class, entry.action),
            (AdoptClass::Extra, AdoptAction::AddIgnoreRule)
        );
        assert_eq!(entry.ignore_rule.as_deref(), Some("config/b.toml"));
        assert_eq!(report.ignore_rules, vec!["config/b.toml"]);
        assert_eq!(report.overlay_copies().count(), 0);
    }

    #[test]
    fn extra_configs_overlay_policy() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a")],
            &[("config/a.toml", b"a"), ("config/b.toml", b"b")],
        );
        let report = fx.classify(&AdoptOptions {
            extra_configs: ExtraConfigPolicy::Overlay,
            ..defaults()
        });
        assert_eq!(
            find(&report, "config/b.toml").action,
            AdoptAction::CopyToOverlay
        );
        assert!(report.ignore_rules.is_empty());
    }

    #[test]
    fn runtime_db_is_ignored_never_overlaid() {
        let fx = Fixture::new(
            &[
                ("config/a.toml", b"a"),
                ("config/mod/a.toml", b"a"),
                ("mods/a.jar", b"a"),
                ("kubejs/a.js", b"a"),
            ],
            &[
                ("config/a.toml", b"a"),
                ("config/mod/a.toml", b"a"),
                ("mods/a.jar", b"a"),
                ("kubejs/a.js", b"a"),
                ("config/mod/data.mv.db", b"database"),
                ("config/cache.sqlite", b"sqlite"),
                ("mods/server.lock", b""),
                ("kubejs/big.bin", &[0u8; 64]),
            ],
        );
        let report = fx.classify(&AdoptOptions {
            extra_configs: ExtraConfigPolicy::Overlay,
            large_file_bytes: 32,
            ..defaults()
        });
        for path in [
            "config/mod/data.mv.db",
            "config/cache.sqlite",
            "mods/server.lock",
            "kubejs/big.bin",
        ] {
            let entry = find(&report, path);
            assert_eq!(entry.class, AdoptClass::RuntimeData, "{path}");
            assert_eq!(entry.action, AdoptAction::AddIgnoreRule, "{path}");
            assert_eq!(
                entry.live.as_ref().unwrap().sha256,
                "",
                "runtime data is never hashed"
            );
        }
        assert_eq!(report.overlay_copies().count(), 0);
        assert!(
            report
                .ignore_rules
                .contains(&"config/mod/data.mv.db".to_string())
        );
    }

    #[test]
    fn fully_extra_directory_collapses_to_one_rule() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a")],
            &[
                ("config/a.toml", b"a"),
                ("config/luckperms/luckperms.conf", b"conf"),
                ("config/luckperms/luckperms-h2-v2.mv.db", b"db"),
                ("config/luckperms/libs/x.jar", b"lib"),
            ],
        );
        let report = fx.classify(&defaults());
        assert_eq!(report.ignore_rules, vec!["config/luckperms"]);
        for path in [
            "config/luckperms/luckperms.conf",
            "config/luckperms/luckperms-h2-v2.mv.db",
            "config/luckperms/libs/x.jar",
        ] {
            assert_eq!(
                find(&report, path).ignore_rule.as_deref(),
                Some("config/luckperms"),
                "{path}"
            );
        }

        // A directory that also holds a pack file is not collapsed.
        let fx = Fixture::new(
            &[("config/mod/pack.toml", b"p")],
            &[
                ("config/mod/pack.toml", b"p"),
                ("config/mod/mine.toml", b"m"),
            ],
        );
        let report = fx.classify(&defaults());
        assert_eq!(report.ignore_rules, vec!["config/mod/mine.toml"]);
    }

    #[test]
    fn missing_mod_gets_ignore_rule() {
        let fx = Fixture::new(
            &[("mods/a.jar", b"a"), ("mods/removed.jar", b"r")],
            &[("mods/a.jar", b"a")],
        );
        let report = fx.classify(&defaults());
        let entry = find(&report, "mods/removed.jar");
        assert_eq!(
            (entry.class, entry.action),
            (AdoptClass::Missing, AdoptAction::AddIgnoreRule)
        );
        assert_eq!(report.ignore_rules, vec!["mods/removed.jar"]);
        assert_eq!(report.missing_ratio, 0.5);
    }

    #[test]
    fn missing_config_restored_by_default() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a"), ("config/gone.toml", b"g")],
            &[("config/a.toml", b"a")],
        );
        let report = fx.classify(&defaults());
        let entry = find(&report, "config/gone.toml");
        assert_eq!(
            (entry.class, entry.action),
            (AdoptClass::Missing, AdoptAction::RestoreOnNextUpdate)
        );
        assert!(report.ignore_rules.is_empty());
        assert_eq!(report.restore_count(), 1);
    }

    #[test]
    fn keep_removed_ignores_everything() {
        let fx = Fixture::new(
            &[
                ("config/a.toml", b"a"),
                ("config/gone.toml", b"g"),
                ("kubejs/gone.js", b"j"),
            ],
            &[("config/a.toml", b"a")],
        );
        let report = fx.classify(&AdoptOptions {
            keep_removed: true,
            ..defaults()
        });
        assert_eq!(report.restore_count(), 0);
        assert_eq!(
            report.ignore_rules,
            vec!["config/gone.toml", "kubejs/gone.js"]
        );
    }

    #[test]
    fn replaced_mod_detected_by_mods_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old.jar");
        let new = tmp.path().join("new.jar");
        write_mod_jar(&old, "jei", "15.2.0");
        write_mod_jar(&new, "jei", "15.3.0");
        // File names that share nothing: only the metadata pairs them.
        let mut fx = Fixture::new(&[], &[]);
        fx.stage("mods/jei-15.2.0.jar", &old);
        let new_bytes = std::fs::read(&new).unwrap();
        write_files(&fx.root, &[("mods/justenoughitems-new.jar", &new_bytes)]);

        let report = fx.classify(&defaults());
        let entry = find(&report, "mods/justenoughitems-new.jar");
        assert_eq!(
            (entry.class, entry.action),
            (AdoptClass::Replaced, AdoptAction::CopyToOverlay)
        );
        assert_eq!(
            entry.related.as_deref(),
            Some(Path::new("mods/jei-15.2.0.jar"))
        );
        assert_eq!(entry.related_rule.as_deref(), Some("mods/jei-15.2.0.jar"));
        assert_eq!(report.ignore_rules, vec!["mods/jei-15.2.0.jar"]);
        assert!(
            !report
                .entries
                .iter()
                .any(|e| e.class == AdoptClass::Missing),
            "the replaced pack jar is not reported missing"
        );
    }

    #[test]
    fn replaced_mod_fallback_by_filename() {
        let fx = Fixture::new(
            &[("mods/create-1.20.1-0.5.1.jar", b"old create")],
            &[
                ("mods/create-1.20.1-0.5.2.jar", b"new create"),
                ("mods/create_new_age-1.1.2.jar", b"addon"),
            ],
        );
        let report = fx.classify(&defaults());
        let entry = find(&report, "mods/create-1.20.1-0.5.2.jar");
        assert_eq!(entry.class, AdoptClass::Replaced);
        assert_eq!(
            entry.related.as_deref(),
            Some(Path::new("mods/create-1.20.1-0.5.1.jar"))
        );
        // Create: New Age is another mod, not a replacement.
        assert_eq!(
            find(&report, "mods/create_new_age-1.1.2.jar").class,
            AdoptClass::Extra
        );
    }

    #[test]
    fn disabled_mod_ignores_both_paths() {
        let fx = Fixture::new(
            &[("mods/a.jar", b"a"), ("mods/laggy.jar", b"lag")],
            &[("mods/a.jar", b"a"), ("mods/laggy.jar.disabled", b"lag")],
        );
        let report = fx.classify(&defaults());
        let entry = find(&report, "mods/laggy.jar.disabled");
        assert_eq!(
            (entry.class, entry.action),
            (AdoptClass::Disabled, AdoptAction::AddIgnoreRule)
        );
        assert_eq!(entry.related.as_deref(), Some(Path::new("mods/laggy.jar")));
        assert_eq!(
            report.ignore_rules,
            vec!["mods/laggy.jar", "mods/laggy.jar.disabled"]
        );
        assert!(!has_entry(&report, "mods/laggy.jar"));
    }

    #[test]
    fn world_libraries_and_unshipped_dirs_never_scanned() {
        let fx = Fixture::new(
            &[("mods/a.jar", b"a"), ("server.properties", b"motd=pack")],
            &[
                ("mods/a.jar", b"a"),
                ("world/level.dat", b"world"),
                ("libraries/net/x.jar", b"lib"),
                ("plugins/thing.jar", b"plugin"),
                ("server.properties", b"motd=mine"),
                ("overlay/mods/old.jar", b"overlay"),
                (".packctl/state.json", b"{}"),
                ("notes.txt", b"mine"),
            ],
        );
        let report = fx.classify(&defaults());
        assert_eq!(report.scanned_dirs, vec!["mods"]);
        for path in [
            "world/level.dat",
            "libraries/net/x.jar",
            "plugins/thing.jar",
            "overlay/mods/old.jar",
            ".packctl/state.json",
            "notes.txt",
        ] {
            assert!(!has_entry(&report, path), "{path} must not be scanned");
        }
        let properties = find(&report, "server.properties");
        assert_eq!(
            (properties.class, properties.action),
            (AdoptClass::Persistent, AdoptAction::Skip)
        );
    }

    #[test]
    fn already_ignored_paths_are_left_alone() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a")],
            &[
                ("config/a.toml", b"edited"),
                ("config/mine/x.toml", b"x"),
                (".packctlignore", b"config/a.toml\nconfig/mine\n"),
            ],
        );
        let report = fx.classify(&defaults());
        assert!(report.entries.is_empty(), "{:#?}", report.entries);
    }

    #[test]
    fn readopt_reclassifies_paths_the_previous_block_covered() {
        let fx = Fixture::new(
            &[
                ("mods/a.jar", b"a"),
                ("mods/removed.jar", b"r"),
                ("config/a.toml", b"a = 1\n"),
            ],
            &[
                ("mods/a.jar", b"a"),
                ("config/a.toml", b"a = 2\n"),
                ("config/luckperms/lp.db", b"db"),
                ("config/mine/x.toml", b"x"),
                (
                    ".packctlignore",
                    b"config/mine\n\n# >>> packctl adopt\n# old\nconfig/a.toml\n\
                      config/luckperms\nmods/removed.jar\n# <<< packctl adopt\n",
                ),
            ],
        );
        let report = fx.classify(&AdoptOptions {
            modified: ModifiedPolicy::Ignore,
            ..defaults()
        });
        // Every path the old block protected gets a rule in the new one.
        assert_eq!(
            report.ignore_rules,
            vec!["config/a.toml", "config/luckperms", "mods/removed.jar"]
        );
        assert_eq!(find(&report, "config/a.toml").class, AdoptClass::Modified);
        assert_eq!(find(&report, "mods/removed.jar").class, AdoptClass::Missing);
        // The admin's own rules still keep their paths out of the report.
        assert!(!has_entry(&report, "config/mine/x.toml"));

        // Applying keeps those protections and the admin's rule.
        fx.apply(&report).unwrap();
        let content = std::fs::read_to_string(fx.root.join(IGNORE_FILE)).unwrap();
        let rules = parse_rules(&content).unwrap();
        for path in [
            "config/a.toml",
            "config/luckperms/lp.db",
            "mods/removed.jar",
            "config/mine/x.toml",
        ] {
            assert!(rules.is_ignored(Path::new(path)), "{path}: {content}");
        }
        assert_eq!(
            content.matches("# >>> packctl adopt").count(),
            1,
            "{content}"
        );
    }

    #[test]
    fn existing_overlay_file_not_overwritten_reports_conflict() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a"), ("config/b.toml", b"b")],
            &[
                ("config/a.toml", b"live edit"),
                ("config/b.toml", b"same edit"),
                ("overlay/config/a.toml", b"older overlay"),
                ("overlay/config/b.toml", b"same edit"),
            ],
        );
        let report = fx.classify(&defaults());
        assert_eq!(
            find(&report, "config/a.toml").action,
            AdoptAction::KeepOverlay
        );
        assert_eq!(
            find(&report, "config/b.toml").action,
            AdoptAction::KeepOverlay
        );
        assert_eq!(
            report.overlay_conflicts,
            vec![PathBuf::from("config/a.toml")]
        );
        assert_eq!(report.overlay_copies().count(), 0);
        assert!(
            report.warnings.iter().any(|w| w.contains(
                "the overlay already holds a different 'config/a.toml': the next update would \
                 overwrite your live file"
            )),
            "{:?}",
            report.warnings
        );
    }

    #[tokio::test]
    async fn overlay_conflict_is_live_drift_on_the_next_update() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a"), ("config/b.toml", b"b")],
            &[
                ("config/a.toml", b"live edit"),
                ("config/b.toml", b"same edit"),
                ("overlay/config/a.toml", b"older overlay"),
                ("overlay/config/b.toml", b"same edit"),
            ],
        );
        let report = fx.classify(&defaults());
        let state = fx.apply(&report).unwrap().state;
        assert_eq!(
            state.overlay_applied["config/a.toml"],
            ManagedFile::new(sha256_bytes(b"older overlay"), 13)
        );

        let updater = fx.updater();
        let prepared = updater
            .prepare_update(&crate::providers::VersionSelector::Latest)
            .await
            .unwrap();
        assert_eq!(prepared.plan.to_id, "id-1");
        assert!(prepared.needs_force_sync(), "{:#?}", prepared.plan);
        assert_eq!(
            prepared.plan.live_drift_paths(),
            vec![Path::new("config/a.toml")]
        );

        let error = updater
            .execute(&prepared, &crate::core::updater::ExecuteOptions::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("--force-sync"), "{error}");
        assert_eq!(
            std::fs::read(fx.root.join("config/a.toml")).unwrap(),
            b"live edit"
        );
    }

    #[test]
    fn differing_overlay_file_outside_the_scan_warns() {
        let fx = Fixture::new(
            &[("mods/a.jar", b"a")],
            &[
                ("mods/a.jar", b"a"),
                ("custom/motd.txt", b"live"),
                ("overlay/custom/motd.txt", b"overlay"),
                ("overlay/custom/new.txt", b"not on the server yet"),
            ],
        );
        let report = fx.classify(&defaults());
        let overlay_warnings: Vec<&String> = report
            .warnings
            .iter()
            .filter(|w| w.starts_with("the overlay holds a different"))
            .collect();
        assert_eq!(
            overlay_warnings,
            vec![
                "the overlay holds a different 'custom/motd.txt'; the next update writes it over \
                 the live file"
            ]
        );
    }

    #[tokio::test]
    async fn adopt_then_plan_is_a_no_op_with_overlay_files_outside_the_scan() {
        let fx = Fixture::new(
            &[("mods/a.jar", b"a"), ("config/a.toml", b"a")],
            &[
                ("mods/a.jar", b"a"),
                ("config/a.toml", b"a"),
                ("mods/extra.jar", b"extra"),
                // An overlay file outside the folders adopt scans, already
                // applied to the server.
                ("custom/motd.txt", b"hello"),
                ("overlay/custom/motd.txt", b"hello"),
            ],
        );
        let report = fx.classify(&defaults());
        let state = fx.apply(&report).unwrap().state;
        let mut applied: Vec<&str> = state.overlay_applied.keys().map(String::as_str).collect();
        applied.sort();
        assert_eq!(applied, vec!["custom/motd.txt", "mods/extra.jar"]);

        let prepared = fx
            .updater()
            .prepare_update(&crate::providers::VersionSelector::Latest)
            .await
            .unwrap();
        assert!(prepared.plan.is_empty(), "{:#?}", prepared.plan);
        assert!(!prepared.needs_state_commit());
        assert!(!prepared.needs_force_sync());
    }

    #[test]
    fn unapplied_overlay_file_is_not_recorded_as_applied() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a"), ("config/b.toml", b"b")],
            &[
                // The live file still equals the pack; the overlay holds an
                // edit the admin prepared but never applied.
                ("config/a.toml", b"a"),
                ("overlay/config/a.toml", b"prepared"),
                // The live edit is already in the overlay.
                ("config/b.toml", b"same edit"),
                ("overlay/config/b.toml", b"same edit"),
            ],
        );
        let report = fx.classify(&defaults());
        assert_eq!(find(&report, "config/a.toml").class, AdoptClass::Identical);
        assert!(
            report.warnings.iter().any(|w| w.contains(
                "the overlay holds a different 'config/a.toml'; the next update writes it"
            )),
            "{:?}",
            report.warnings
        );
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("different 'config/b.toml'")),
            "{:?}",
            report.warnings
        );

        let state = fx.apply(&report).unwrap().state;
        let mut applied: Vec<&str> = state.overlay_applied.keys().map(String::as_str).collect();
        applied.sort();
        assert_eq!(applied, vec!["config/b.toml"]);

        // The next update applies the prepared overlay file without
        // reporting live drift.
        let plan = crate::core::planner::UpdatePlanner::new(FilePolicy::default_policy())
            .with_reserved(ReservedPaths::for_profile(&fx.profile))
            .build_plan_for_server(&state, &fx.prepared, &fx.overlay(), &fx.root)
            .unwrap();
        assert!(plan.live_drift_paths().is_empty(), "{plan:#?}");
        assert!(!plan.needs_force_sync());
        let pending: Vec<&Path> = plan
            .overlay_changes
            .iter()
            .filter(|change| change.pending)
            .map(|change| change.rel_path.as_path())
            .collect();
        assert_eq!(pending, vec![Path::new("config/a.toml")]);
    }

    #[test]
    fn unexpressible_runtime_data_falls_back_to_a_directory_rule() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a")],
            &[
                ("config/a.toml", b"a"),
                ("config/stats/run#1.log", b"log"),
                ("config/odd#name.db", b"db"),
            ],
        );
        let report = fx.classify(&defaults());
        // Inside a directory without pack files: that directory is ignored.
        assert_eq!(
            find(&report, "config/stats/run#1.log")
                .ignore_rule
                .as_deref(),
            Some("config/stats")
        );
        // Directly in a pack folder: nothing can protect it.
        let odd = find(&report, "config/odd#name.db");
        assert_eq!(odd.action, AdoptAction::Skip);
        assert_eq!(
            report.unprotected,
            vec![PathBuf::from("config/odd#name.db")]
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("cannot express an ignore rule for 'config/odd#name.db'"))
        );
        // Applying refuses, so the data is never scheduled for deletion.
        let error = fx.apply(&report).unwrap_err();
        assert!(error.to_string().contains("config/odd#name.db"), "{error}");
        assert!(!fx.root.join(".packctl/state.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_skipped() {
        let fx = Fixture::new(&[("mods/a.jar", b"a")], &[("mods/a.jar", b"a")]);
        std::os::unix::fs::symlink("/etc/passwd", fx.root.join("mods/link.jar")).unwrap();
        std::os::unix::fs::symlink("/etc", fx.root.join("mods/linkdir")).unwrap();
        let report = fx.classify(&defaults());
        assert!(!has_entry(&report, "mods/link.jar"));
        assert!(
            !report
                .entries
                .iter()
                .any(|entry| entry.rel_path.starts_with("mods/linkdir"))
        );
    }

    /// A server with every kind of change.
    fn mixed_fixture() -> Fixture {
        Fixture::new(
            &[
                ("mods/a.jar", b"a"),
                ("mods/removed.jar", b"r"),
                ("config/a.toml", b"a = 1\n"),
                ("config/edited.toml", b"x = 1\n"),
            ],
            &[
                ("mods/a.jar", b"a"),
                ("mods/added.jar", b"added"),
                ("config/a.toml", b"a = 1\n"),
                ("config/edited.toml", b"x = 2\n"),
                ("config/luckperms/lp.db", b"db"),
                ("world/level.dat", b"world"),
            ],
        )
    }

    #[test]
    fn dry_run_changes_nothing() {
        let fx = mixed_fixture();
        let before = tree_snapshot(fx.tmp.path());
        let report = fx.classify(&defaults());
        assert!(!report.entries.is_empty());
        assert_eq!(tree_snapshot(fx.tmp.path()), before);
    }

    #[test]
    fn apply_writes_overlay_ignore_block_and_state_with_adoption_record() {
        let fx = mixed_fixture();
        write_files(&fx.root, &[(".packctlignore", b"# mine\nlogs-extra\n")]);
        let report = fx.classify(&defaults());
        let live_before = tree_snapshot(&fx.root.join("mods"));

        let outcome = fx.apply(&report).unwrap();

        // Overlay copies.
        let overlay = fx.overlay_root();
        assert_eq!(
            std::fs::read(overlay.join("mods/added.jar")).unwrap(),
            b"added"
        );
        assert_eq!(
            std::fs::read(overlay.join("config/edited.toml")).unwrap(),
            b"x = 2\n"
        );
        assert_eq!(outcome.overlay_written.len(), 2);
        // The live pack folders are untouched.
        assert_eq!(tree_snapshot(&fx.root.join("mods")), live_before);

        // The ignore file keeps the admin's rules and gains the block.
        let ignore = std::fs::read_to_string(fx.root.join(IGNORE_FILE)).unwrap();
        assert!(
            ignore.starts_with("# mine\nlogs-extra\n\n# >>> packctl adopt\n# "),
            "{ignore}"
        );
        assert!(ignore.contains(" 1.0 (id id-1)\n"), "{ignore}");
        assert!(
            ignore.ends_with("config/luckperms\nmods/removed.jar\n# <<< packctl adopt\n"),
            "{ignore}"
        );
        parse_rules(&ignore).unwrap();

        // The state.
        let state = StateStore::at(&fx.root).unwrap().load().unwrap();
        assert_eq!(state, outcome.state);
        assert_eq!(state.installed_version.as_deref(), Some("1.0"));
        assert_eq!(state.provider_version_id.as_deref(), Some("id-1"));
        assert_eq!(state.provider.as_deref(), Some("curseforge"));
        assert_eq!(state.project.as_deref(), Some("42"));
        assert_eq!(state.last_successful_update, None);
        let mut managed: Vec<&str> = state.managed_files.keys().map(String::as_str).collect();
        managed.sort();
        assert_eq!(managed, vec!["config/a.toml", "mods/a.jar"]);
        assert_eq!(
            state.overlay_shadowed.keys().collect::<Vec<_>>(),
            vec!["config/edited.toml"]
        );
        assert_eq!(state.overlay_applied.len(), 2);
        let record = state.adopted.as_ref().unwrap();
        assert_eq!(record.version, "1.0");
        assert_eq!(record.provider_version_id, "id-1");
        assert_eq!(
            record.ignore_rules,
            vec!["config/luckperms", "mods/removed.jar"]
        );
        let mut added = record.overlay_added.clone();
        added.sort();
        assert_eq!(added, vec!["config/edited.toml", "mods/added.jar"]);

        // Adopting again needs --force.
        let error = fx.apply(&report).unwrap_err();
        assert!(
            error.to_string().contains("already has packctl state"),
            "{error}"
        );
    }

    #[test]
    fn apply_snapshot_then_rollback_removes_overlay_files_and_restores_state() {
        let fx = mixed_fixture();
        let before = tree_snapshot(&fx.root);
        let report = fx.classify(&defaults());
        let outcome = fx.apply(&report).unwrap();
        assert!(fx.overlay_root().join("mods/added.jar").exists());

        let snapshots = list_snapshots(&fx.root).unwrap();
        let (target, _) = rollback_target(&snapshots).unwrap();
        assert_eq!(target.dir, outcome.snapshot.dir);
        restore_snapshot(&fx.root, target).unwrap();
        mark_restored(target).unwrap();

        // Everything outside packctl's own directory is as before.
        let after: Vec<(String, Vec<u8>)> = tree_snapshot(&fx.root)
            .into_iter()
            .filter(|(rel, _)| !rel.starts_with(".packctl/"))
            .collect();
        assert_eq!(after, before);
        assert!(StateStore::at(&fx.root).unwrap().load().unwrap().is_empty());
        // A second rollback has nothing left to undo.
        assert!(rollback_target(&list_snapshots(&fx.root).unwrap()).is_none());
    }

    #[test]
    fn rollback_keeps_overlay_files_adopt_did_not_write() {
        let fx = Fixture::new(
            &[("config/a.toml", b"a")],
            &[
                ("config/a.toml", b"edited"),
                ("mods/added.jar", b"added"),
                ("overlay/config/a.toml", b"edited"),
                ("overlay/config/admin.toml", b"admin"),
            ],
        );
        let report = fx.classify(&defaults());
        assert_eq!(
            find(&report, "config/a.toml").action,
            AdoptAction::KeepOverlay
        );
        let outcome = fx.apply(&report).unwrap();
        let tracked = &outcome.snapshot.manifest.tracked_paths;
        assert!(
            !tracked
                .iter()
                .any(|path| path.contains("config/a.toml") || path.contains("admin.toml")),
            "{tracked:?}"
        );

        restore_snapshot(&fx.root, &outcome.snapshot).unwrap();
        let overlay = fx.overlay_root();
        assert_eq!(
            std::fs::read(overlay.join("config/a.toml")).unwrap(),
            b"edited"
        );
        assert_eq!(
            std::fs::read(overlay.join("config/admin.toml")).unwrap(),
            b"admin"
        );
        assert!(
            !overlay.join("mods/added.jar").exists(),
            "adopt's own copy is removed"
        );
    }

    #[test]
    fn apply_failure_midway_removes_written_overlay_files() {
        let fx = Fixture::new(
            &[("mods/a.jar", b"a")],
            &[
                ("mods/a.jar", b"a"),
                ("mods/first.jar", b"first"),
                ("mods/second.jar", b"second"),
            ],
        );
        write_files(&fx.root, &[(".packctlignore", b"mine\n")]);
        let report = fx.classify(&defaults());
        // The server rewrites a file after it was classified.
        write_files(&fx.root, &[("mods/second.jar", b"changed!")]);

        let error = fx.apply(&report).unwrap_err().to_string();
        assert!(error.contains("changed while adopting"), "{error}");
        assert!(error.contains("nothing was changed"), "{error}");
        assert!(!fx.overlay_root().join("mods/first.jar").exists());
        assert!(!fx.overlay_root().join("mods/second.jar").exists());
        assert_eq!(std::fs::read(fx.root.join(IGNORE_FILE)).unwrap(), b"mine\n");
        assert!(!fx.root.join(".packctl/state.json").exists());
        assert!(
            list_snapshots(&fx.root).unwrap().is_empty(),
            "no snapshot is left behind"
        );
    }

    #[test]
    fn apply_refuses_an_overlay_outside_the_root_without_force() {
        let mut fx = mixed_fixture();
        let outside = fx.tmp.path().join("shared-overlay");
        fx.profile.overlay.path = outside.clone();
        let report = fx.classify(&defaults());
        let error = fx.apply(&report).unwrap_err().to_string();
        assert!(error.contains("outside the server root"), "{error}");
        assert!(!outside.exists());

        let outcome = apply_adoption(
            &report,
            &fx.prepared,
            &fx.profile,
            &[],
            &ApplyOptions { force: true },
        )
        .unwrap();
        assert!(outside.join("mods/added.jar").exists());
        assert_eq!(outcome.overlay_written.len(), 2);
    }

    #[test]
    fn version_name_cannot_inject_ignore_rules() {
        let fx = Fixture::new(
            &[("mods/a.jar", b"a"), ("mods/removed.jar", b"r")],
            &[("mods/a.jar", b"a")],
        );
        let mut report = fx.classify(&defaults());
        report.version.name = "evil\nmods\n# <<< packctl adopt".to_string();
        assert!(
            fx.apply(&report).is_err(),
            "a marker in the comment is refused"
        );

        report.version.name = "evil\nmods\n**".to_string();
        fx.apply(&report).unwrap();
        let ignore = std::fs::read_to_string(fx.root.join(IGNORE_FILE)).unwrap();
        let rules = parse_rules(&ignore).unwrap();
        assert!(!rules.is_ignored(Path::new("mods/a.jar")), "{ignore}");
        assert!(rules.is_ignored(Path::new("mods/removed.jar")));
    }

    #[test]
    fn report_entries_are_sorted_by_class_then_path() {
        let report = mixed_fixture().classify(&defaults());
        let order: Vec<(AdoptClass, &Path)> = report
            .entries
            .iter()
            .map(|entry| (entry.class, entry.rel_path.as_path()))
            .collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
    }
}
