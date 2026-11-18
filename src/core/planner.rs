//! Update planner.
//!
//! The planner compares the current managed state against the desired staged
//! upstream version, the mirrored local overlay, and the persistent-data
//! policy, and produces a complete, serializable [`UpdatePlan`]. It never
//! mutates the live server. It is the single planning path shared by both
//! `packctl plan` and `packctl update` (see design notes "Update Planner").
//!
//! The model is upstream + overlay + persistent data = running server. The
//! selected upstream version is authoritative for pack-managed content,
//! the overlay always wins over upstream, and persistent runtime data is never
//! touched merely because it is absent from a new pack version.
//!
//! Live files are compared by content, so a file that already matches what
//! the plan would write is never planned, even when packctl has no record of
//! it (the first update of a hand-installed server). Changes that would
//! destroy something packctl never wrote are flagged on the plan
//! ([`UpdatePlan::unmanaged_removals`] and [`OverlayChange::live_drift`]) so
//! the updater can refuse them unless the user forces the sync.
//!
//! The public API in this module is consumed by the executor and CLI modules.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::ignore::IgnoreRules;
use crate::core::overlay::OverlayFile;
use crate::core::ownership::FilePolicy;
use crate::core::reserved::{ReservedPaths, skipped_notice};
use crate::core::state::{InstalledState, ManagedFile};
use crate::error::{PackError, Result};
use crate::fs::hashing::sha256_file;
use crate::fs::paths::{normalize_relative, safe_join};
use crate::providers::{PackRequirements, PreparedFile, PreparedPack};

/// What should happen to a managed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// The file is new to the upstream version and must be written.
    Add,
    /// An existing file must be overwritten with new content.
    Replace,
    /// A file disappeared upstream and must be removed.
    Remove,
}

/// One upstream file change in an [`UpdatePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Destination path relative to the server root (forward slashes).
    pub rel_path: PathBuf,
    pub kind: ChangeKind,
    /// Where the new content comes from (staged upstream file, or overlay
    /// file). None for removals.
    pub source: Option<PathBuf>,
    pub sha256: Option<String>,
}

/// How an overlay file relates to previously installed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayChangeStatus {
    /// The overlay file is brand new to the server (nothing was managed there).
    Applied,
    /// The overlay replaces a pack file whose upstream content did not change;
    /// the overlay is simply re-applied.
    ReplacesUnchanged,
    /// The overlay replaces a pack file whose upstream content changed;
    /// surfaced as an informational conflict.
    ReplacesChanged,
}

/// One overlay file in an [`UpdatePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayChange {
    /// Destination path relative to the server root (forward slashes).
    pub rel_path: PathBuf,
    pub status: OverlayChangeStatus,
    /// True when the live file differs from the overlay file, so applying
    /// the overlay writes it. An overlay file that is already in place is
    /// not pending and does not make the plan non-empty.
    #[serde(default)]
    pub pending: bool,
    /// True when applying the overlay would overwrite live changes packctl
    /// did not make: the live file differs from the overlay file and from the
    /// overlay content packctl last wrote there (or, on a server packctl has
    /// no record of, the live file simply differs).
    #[serde(default)]
    pub live_drift: bool,
}

/// An informational note attached to a plan (never an error in V1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNotice {
    /// The path the notice is about, when it concerns a specific file.
    pub path: Option<PathBuf>,
    pub message: String,
    /// Shown only with `--verbose`: detail that is not worth a warning.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verbose_only: bool,
}

impl PlanNotice {
    /// A notice about the plan as a whole, always shown.
    pub fn general(message: String) -> Self {
        PlanNotice {
            path: None,
            message,
            verbose_only: false,
        }
    }
}

/// A complete, serializable representation of the work an update intends to do.
///
/// The planner builds it; both the plan and update commands consume the exact
/// same plan (dry-run preview and execution must never diverge).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePlan {
    pub from_version: Option<String>,
    pub from_id: Option<String>,
    pub to_version: String,
    pub to_id: String,
    pub additions: Vec<FileChange>,
    pub modifications: Vec<FileChange>,
    pub removals: Vec<FileChange>,
    pub overlay_changes: Vec<OverlayChange>,
    pub notices: Vec<PlanNotice>,
    /// The removals of files packctl never installed: paths that are neither
    /// recorded as upstream-managed nor owned by the overlay in the installed
    /// state. A subset of `removals`, listed so they can be confirmed.
    #[serde(default)]
    pub unmanaged_removals: Vec<PathBuf>,
    /// The pack files that would overwrite a differing live file packctl
    /// has no record of writing, on a server it already manages (an admin
    /// added that file by hand). A subset of `modifications`; a server with
    /// no state at all is covered by the first-update guard instead.
    #[serde(default)]
    pub unmanaged_overwrites: Vec<PathBuf>,
    /// What the target version needs from the server (Minecraft and loader
    /// versions), when the provider reports it.
    #[serde(default)]
    pub requirements: Option<PackRequirements>,
}

impl UpdatePlan {
    /// True when there is no file work to perform at all.
    ///
    /// Notices alone (informational) and overlay files that are already in
    /// place never make a plan non-empty.
    pub fn is_empty(&self) -> bool {
        self.additions.is_empty()
            && self.modifications.is_empty()
            && self.removals.is_empty()
            && self.overlay_changes.iter().all(|change| !change.pending)
    }

    /// How many upstream (non-overlay) file changes the plan contains.
    pub fn upstream_change_count(&self) -> usize {
        self.additions.len() + self.modifications.len() + self.removals.len()
    }

    /// How many overlay files would actually be written.
    pub fn pending_overlay_count(&self) -> usize {
        self.overlay_changes
            .iter()
            .filter(|change| change.pending)
            .count()
    }

    /// Overlay targets whose live content would be overwritten although
    /// packctl did not write it.
    pub fn live_drift_paths(&self) -> Vec<&Path> {
        self.overlay_changes
            .iter()
            .filter(|change| change.live_drift)
            .map(|change| change.rel_path.as_path())
            .collect()
    }

    /// True when executing the plan would delete or overwrite files packctl
    /// never installed, or overwrite live changes it did not make. Such a
    /// plan only runs when the user forces the sync.
    pub fn needs_force_sync(&self) -> bool {
        !self.unmanaged_removals.is_empty()
            || !self.unmanaged_overwrites.is_empty()
            || self.overlay_changes.iter().any(|change| change.live_drift)
    }

    /// True when executing the plan changes the recorded state: there is
    /// file work, the target version differs from the recorded one, the
    /// state the update would record (`next`) tracks different files than
    /// `from`, or `next` knows the Minecraft version and `from` records
    /// another one (or none, as state written before it was recorded).
    ///
    /// A plan without file work that still needs a commit only records the
    /// new state; the server is not touched.
    pub fn needs_state_commit(&self, from: &InstalledState, next: &InstalledState) -> bool {
        !self.is_empty()
            || from.provider_version_id.as_deref() != Some(self.to_id.as_str())
            || from.installed_version.as_deref() != Some(self.to_version.as_str())
            || from.managed_files != next.managed_files
            || from.overlay_shadowed != next.overlay_shadowed
            || from.overlay_applied != next.overlay_applied
            || (next.minecraft.is_some() && from.minecraft != next.minecraft)
    }
}

/// Builds [`UpdatePlan`]s from installed state, prepared upstream, and overlay.
pub struct UpdatePlanner {
    pub policy: FilePolicy,
    pub ignore: IgnoreRules,
    pub reserved: ReservedPaths,
}

impl UpdatePlanner {
    pub fn new(policy: FilePolicy) -> Self {
        UpdatePlanner {
            policy,
            ignore: IgnoreRules::default(),
            reserved: ReservedPaths::default_layout(),
        }
    }

    /// Restricts the plan with `.packctlignore` rules: matched paths are never
    /// removed by the sweep and never updated from the upstream pack.
    pub fn with_ignore(mut self, ignore: IgnoreRules) -> Self {
        self.ignore = ignore;
        self
    }

    /// Sets the paths pack content may never write to or sweep (packctl's
    /// control files, the overlay and the drop folder). Defaults to
    /// [`ReservedPaths::default_layout`].
    pub fn with_reserved(mut self, reserved: ReservedPaths) -> Self {
        self.reserved = reserved;
        self
    }

    /// Conservative plan that treats the recorded managed state as the only
    /// candidate set for removals.
    ///
    /// This form does not walk the live server, so it cannot sweep unknown
    /// files under pack-owned folders. It exists for pure unit tests and
    /// callers that have no server root; authoritative planning goes through
    /// [`UpdatePlanner::build_plan_for_server`].
    pub fn build_plan(
        &self,
        from: &InstalledState,
        desired: &PreparedPack,
        overlay: &[OverlayFile],
    ) -> Result<UpdatePlan> {
        let live: Vec<PathBuf> = from.managed_files.keys().map(PathBuf::from).collect();
        self.build_plan_with_inputs(from, desired, overlay, &live, None)
    }

    /// Builds a plan against a live server root.
    ///
    /// Pack-owned folders (the top-level directories shipped by the selected
    /// upstream version) are swept: any file in them that is neither part of
    /// the new upstream version nor provided by the overlay nor protected by
    /// persistent-data policy is removed. This keeps the server exactly equal
    /// to upstream + overlay even when no prior managed state exists. Files
    /// outside pack-owned folders (e.g. `libraries/`, `world/`, unknown
    /// top-level files) and packctl's reserved paths are never touched,
    /// except the pack files packctl recorded installing there: a file an
    /// earlier version shipped in a folder the new version dropped (or under
    /// a wrapper folder an older packctl did not strip) is removed like any
    /// other file upstream no longer ships, instead of being left behind and
    /// forgotten.
    pub fn build_plan_for_server(
        &self,
        from: &InstalledState,
        desired: &PreparedPack,
        overlay: &[OverlayFile],
        server_root: &Path,
    ) -> Result<UpdatePlan> {
        let mut live = Vec::new();
        for dir in pack_owned_dirs(desired, &self.reserved) {
            collect_live_files(
                server_root,
                Path::new(&dir),
                &self.policy,
                &self.ignore,
                &self.reserved,
                &mut live,
            )?;
        }
        let mut seen: HashSet<String> = live.iter().map(|rel| rel_key(rel)).collect();
        for rel in recorded_live_files(server_root, from)? {
            if seen.insert(rel_key(&rel)) {
                live.push(rel);
            }
        }
        self.build_plan_with_inputs(from, desired, overlay, &live, Some(server_root))
    }

    fn build_plan_with_inputs(
        &self,
        from: &InstalledState,
        desired: &PreparedPack,
        overlay: &[OverlayFile],
        live: &[PathBuf],
        server_root: Option<&Path>,
    ) -> Result<UpdatePlan> {
        let desired_files: HashMap<String, &PreparedFile> = desired
            .files
            .iter()
            .map(|file| (rel_key(&file.rel_path), file))
            .collect();
        let overlay_files: HashMap<String, &OverlayFile> = overlay
            .iter()
            .map(|file| (rel_key(&file.rel_path), file))
            .collect();

        let mut additions = Vec::new();
        let mut modifications = Vec::new();
        let mut removals = Vec::new();
        let mut unmanaged_removals = Vec::new();
        let mut unmanaged_overwrites = Vec::new();
        let mut notices: Vec<PlanNotice> = desired
            .notices
            .iter()
            .map(|message| PlanNotice {
                path: None,
                verbose_only: false,
                message: message.clone(),
            })
            .collect();

        // Rule 1: removals.
        //
        // Every file present in a pack-owned folder that the selected upstream
        // version does not ship is removed, unless it is protected by
        // persistent-data policy, ignored, reserved, or provided by the
        // overlay (overlay wins, so it stays on disk and is re-applied). This
        // sweep applies to all files in those folders, regardless of prior
        // managed state, so the update converges exactly onto upstream +
        // overlay. Removals of files packctl has no record of are listed in
        // `unmanaged_removals` so they can be confirmed first.
        for path in live {
            let rel = path.as_path();
            if self.policy.is_persistent(rel)
                || self.ignore.is_ignored(rel)
                || self.reserved.is_reserved(rel)
            {
                continue;
            }
            let key = rel_key(rel);
            if overlay_files.contains_key(&key) {
                continue;
            }
            if !desired_files.contains_key(&key) {
                if !was_installed_by_packctl(from, &key) {
                    unmanaged_removals.push(PathBuf::from(&key));
                }
                removals.push(FileChange {
                    rel_path: PathBuf::from(&key),
                    kind: ChangeKind::Remove,
                    source: None,
                    sha256: None,
                });
            }
        }

        // Rule 2: additions / modifications.
        //
        // Never manage content under persistent, ignored or reserved paths.
        // When the overlay provides the same path, the overlay change handles
        // it instead of the upstream content being staged here.
        let mut reserved_skipped = Vec::new();
        for (path, desired_file) in &desired_files {
            let rel = Path::new(path);
            if self.policy.is_persistent(rel) || self.ignore.is_ignored(rel) {
                continue;
            }
            if self.reserved.is_reserved(rel) {
                reserved_skipped.push(path.clone());
                continue;
            }
            if overlay_files.contains_key(path) {
                continue;
            }
            let source = desired.root.join(&desired_file.rel_path);
            let change = |kind| FileChange {
                rel_path: desired_file.rel_path.clone(),
                kind,
                source: Some(source.clone()),
                sha256: Some(desired_file.sha256.clone()),
            };
            match from.managed_files.get(path) {
                Some(managed) if managed.sha256 != desired_file.sha256 => {
                    modifications.push(change(ChangeKind::Replace));
                }
                Some(managed) if live_file_matches(server_root, rel, managed)? => {}
                // Upstream is unchanged and the live file is the local
                // variant accepted at adoption (only reformatted).
                Some(ManagedFile {
                    local_sha256: Some(local),
                    ..
                }) if live_sha256_is(server_root, rel, local)? => {}
                // A managed file missing from disk or altered outside packctl
                // must be restored from the staged authoritative content.
                Some(_) => modifications.push(change(ChangeKind::Replace)),
                // Not recorded: compare with what is on disk, so identical
                // files on a hand-installed server are left alone.
                None => match server_root {
                    None => additions.push(change(ChangeKind::Add)),
                    Some(root) => {
                        match probe_live(root, rel, desired_file.size, &desired_file.sha256)? {
                            LiveFile::Missing => additions.push(change(ChangeKind::Add)),
                            LiveFile::Matches => {}
                            LiveFile::Differs | LiveFile::NotAFile => {
                                // On a managed server, a differing file
                                // packctl never wrote was put there by hand
                                // (say a config a hand-added mod generated).
                                // An empty state is the first-update guard's
                                // job instead.
                                if !from.is_empty() && !was_installed_by_packctl(from, path) {
                                    unmanaged_overwrites.push(PathBuf::from(path));
                                }
                                modifications.push(change(ChangeKind::Replace));
                            }
                        }
                    }
                },
            }
        }
        if !reserved_skipped.is_empty() {
            reserved_skipped.sort();
            notices.push(PlanNotice {
                path: None,
                verbose_only: false,
                message: skipped_notice(&reserved_skipped),
            });
        }

        // Rule 4: overlay changes.
        //
        // Overlay files always win. The previous upstream content of a path
        // is its managed fingerprint or, when the overlay already owned it,
        // its shadowed fingerprint; if upstream changed since, an
        // informational conflict notice is raised (on every update, not just
        // the first one after the overlay took over the path).
        let mut overlay_changes = Vec::new();
        for overlay_file in overlay {
            let path = rel_key(&overlay_file.rel_path);
            let previous = from
                .managed_files
                .get(&path)
                .or_else(|| from.overlay_shadowed.get(&path));
            let status = match previous {
                Some(previous) => {
                    let upstream_changed = desired_files
                        .get(&path)
                        .is_some_and(|desired| desired.sha256 != previous.sha256);
                    if upstream_changed {
                        notices.push(PlanNotice {
                            path: Some(overlay_file.rel_path.clone()),
                            verbose_only: false,
                            message: conflict_notice(&overlay_file.rel_path),
                        });
                        OverlayChangeStatus::ReplacesChanged
                    } else {
                        OverlayChangeStatus::ReplacesUnchanged
                    }
                }
                None => OverlayChangeStatus::Applied,
            };
            let (pending, live_drift) = match server_root {
                None => (true, false),
                Some(root) => {
                    let rel = overlay_file.rel_path.as_path();
                    match probe_live(root, rel, overlay_file.size, &overlay_file.sha256)? {
                        LiveFile::Matches => (false, false),
                        LiveFile::Missing | LiveFile::NotAFile => (true, false),
                        LiveFile::Differs => {
                            let drift = match from.overlay_applied.get(&path) {
                                Some(applied) => {
                                    !live_matches_hash(root, rel, applied.size, &applied.sha256)?
                                }
                                None => from.is_empty(),
                            };
                            (true, drift)
                        }
                    }
                }
            };
            overlay_changes.push(OverlayChange {
                rel_path: overlay_file.rel_path.clone(),
                status,
                pending,
                live_drift,
            });
        }

        // Rule 6: deterministic output.
        additions.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        modifications.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        removals.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        unmanaged_removals.sort();
        unmanaged_overwrites.sort();
        overlay_changes.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

        Ok(UpdatePlan {
            from_version: from.installed_version.clone(),
            from_id: from.provider_version_id.clone(),
            to_version: desired.version.name.clone(),
            to_id: desired.version.id.clone(),
            additions,
            modifications,
            removals,
            overlay_changes,
            notices,
            unmanaged_removals,
            unmanaged_overwrites,
            requirements: desired.requirements.clone(),
        })
    }
}

/// True when the installed state shows packctl wrote `key`: as an upstream
/// file, as a pack path the overlay owns, or as an overlay file.
fn was_installed_by_packctl(from: &InstalledState, key: &str) -> bool {
    from.managed_files.contains_key(key)
        || from.overlay_shadowed.contains_key(key)
        || from.overlay_applied.contains_key(key)
}

/// The pack files the installed state records packctl installing (as
/// upstream files, or as pack paths the overlay owns) that are still regular
/// files on disk, reached without crossing a symlink. A recorded key that is
/// not a safe relative path is skipped.
fn recorded_live_files(server_root: &Path, from: &InstalledState) -> Result<Vec<PathBuf>> {
    let mut keys: Vec<&String> = from
        .managed_files
        .keys()
        .chain(from.overlay_shadowed.keys())
        .collect();
    keys.sort();
    keys.dedup();
    let mut files = Vec::new();
    for key in keys {
        let Ok(rel) = normalize_relative(Path::new(key)) else {
            continue;
        };
        if is_regular_file_without_symlinks(server_root, &rel)? {
            files.push(rel);
        }
    }
    Ok(files)
}

/// True when `root/rel` is a regular file and no component of the path
/// below `root` is a symlink.
fn is_regular_file_without_symlinks(root: &Path, rel: &Path) -> Result<bool> {
    let mut current = root.to_path_buf();
    let mut components = rel.components().peekable();
    while let Some(component) = components.next() {
        current.push(component);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(PackError::io(
                    format!("stat '{}'", current.display()),
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            return Ok(false);
        }
        if components.peek().is_none() {
            return Ok(metadata.is_file());
        }
        if !metadata.is_dir() {
            return Ok(false);
        }
    }
    Ok(false)
}

/// Top-level directories the prepared upstream pack ships; these are the
/// pack-owned regions swept during a sync update. Reserved directories are
/// excluded so the overlay, the drop folder, and the state directory are
/// never treated as pack content.
pub(crate) fn pack_owned_dirs(desired: &PreparedPack, reserved: &ReservedPaths) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for file in &desired.files {
        let mut components = file.rel_path.components();
        let Some(first) = components.next() else {
            continue;
        };
        if components.next().is_none() {
            continue;
        }
        if let Some(name) = first.as_os_str().to_str()
            && !reserved.is_reserved(Path::new(name))
        {
            dirs.insert(name.to_string());
        }
    }
    dirs.into_iter().collect()
}

/// The files the prepared upstream pack ships directly in the server root
/// (such as `user_jvm_args.txt`). They live outside every pack-owned
/// folder, so the sweep never scans for them.
pub(crate) fn pack_top_level_files(desired: &PreparedPack) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = desired
        .files
        .iter()
        .filter(|file| file.rel_path.components().count() == 1)
        .map(|file| file.rel_path.clone())
        .collect();
    files.sort();
    files
}

/// Collects every regular file under `root/rel_dir` as a relative path.
///
/// Symlinks are never followed (and never returned), so a destructive sweep
/// cannot cross or remove them. Persistent, ignored and reserved subtrees are
/// skipped entirely so a `world/` or `logs/` living under a pack-owned folder,
/// runtime data protected by `.packctlignore`, or an overlay nested inside a
/// pack folder is never walked or removed.
pub(crate) fn collect_live_files(
    root: &Path,
    rel_dir: &Path,
    policy: &FilePolicy,
    ignore: &IgnoreRules,
    reserved: &ReservedPaths,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    let dir = if rel_dir.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_dir)
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PackError::io(format!("scan '{}'", dir.display()), error)),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Err(PackError::io(format!("scan '{}'", dir.display()), error)),
        };
        let path = entry.path();
        let rel = rel_dir.join(entry.file_name());
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(PackError::io(format!("stat '{}'", path.display()), error)),
        };
        if metadata.file_type().is_symlink() || reserved.is_reserved(&rel) {
            continue;
        }
        if metadata.is_dir() {
            if !policy.is_persistent(&rel) && !ignore.is_ignored(&rel) {
                collect_live_files(root, &rel, policy, ignore, reserved, out)?;
            }
            continue;
        }
        if metadata.is_file() && !ignore.is_ignored(&rel) {
            out.push(rel);
        }
    }
    Ok(())
}

/// What is on disk at a planned destination, compared with wanted content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveFile {
    /// Nothing exists at the path.
    Missing,
    /// A regular file with the wanted size and sha256.
    Matches,
    /// A regular file with other content.
    Differs,
    /// Something other than a regular file (a directory or a symlink).
    NotAFile,
}

/// Compares the live file at `root/rel` with the wanted size and hash.
///
/// Uses `symlink_metadata`, so a symlink never counts as a match. The hash is
/// only computed when the size already matches.
fn probe_live(root: &Path, rel: &Path, size: u64, sha256: &str) -> Result<LiveFile> {
    let path = safe_join(root, rel)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LiveFile::Missing);
        }
        Err(error) => {
            return Err(PackError::io(
                format!("stat live file '{}'", path.display()),
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(LiveFile::NotAFile);
    }
    if metadata.len() == size && sha256_file(&path)? == sha256 {
        Ok(LiveFile::Matches)
    } else {
        Ok(LiveFile::Differs)
    }
}

/// True when the live file at `server_root/rel` is a regular file with the
/// given size and sha256.
pub(crate) fn live_matches_hash(
    server_root: &Path,
    rel: &Path,
    size: u64,
    sha256: &str,
) -> Result<bool> {
    Ok(probe_live(server_root, rel, size, sha256)? == LiveFile::Matches)
}

/// True when the live file at `server_root/rel` is a regular file whose
/// sha256 is `sha256` (any size). Without a server root nothing is live.
fn live_sha256_is(server_root: Option<&Path>, rel: &Path, sha256: &str) -> Result<bool> {
    let Some(root) = server_root else {
        return Ok(false);
    };
    let path = safe_join(root, rel)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(sha256_file(&path)? == sha256),
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PackError::io(
            format!("stat live file '{}'", path.display()),
            error,
        )),
    }
}

fn live_file_matches(
    server_root: Option<&Path>,
    rel: &Path,
    managed: &ManagedFile,
) -> Result<bool> {
    match server_root {
        None => Ok(true),
        Some(root) => live_matches_hash(root, rel, managed.size, &managed.sha256),
    }
}

/// Converts a relative path to the forward-slash string key used by managed
/// state. Inputs are pre-normalized by the provider and overlay layers; the
/// backslash replacement is a defensive fallback.
fn rel_key(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Builds the informational overlay conflict notice (see design notes "Overlay
/// Wins"). Informational only; strict mode is a future feature.
fn conflict_notice(rel_path: &Path) -> String {
    format!(
        "Overlay conflict notice\n~ {}\n  Upstream changed this file.\n  Local overlay replaces it.\n  Overlay version will be used.",
        rel_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ignore::parse_rules;
    use crate::core::state::ManagedFile;
    use crate::fs::hashing::sha256_bytes;
    use crate::providers::PackVersion;

    const CONFLICT_NOTICE: &str = "Overlay conflict notice\n~ {path}\n  Upstream changed this file.\n  Local overlay replaces it.\n  Overlay version will be used.";

    fn managed(sha: &str, size: u64) -> ManagedFile {
        ManagedFile::new(sha, size)
    }

    fn staged(rel: &str, sha: &str) -> PreparedFile {
        PreparedFile {
            rel_path: PathBuf::from(rel),
            size: sha.len() as u64,
            sha256: sha.to_string(),
        }
    }

    fn overlay_file(rel: &str, sha: &str) -> OverlayFile {
        OverlayFile {
            rel_path: PathBuf::from(rel),
            source: PathBuf::from(format!("/overlay/{rel}")),
            sha256: sha.to_string(),
            size: sha.len() as u64,
        }
    }

    fn pack(version: &str, id: &str, files: Vec<PreparedFile>) -> PreparedPack {
        PreparedPack {
            name: "fixture-pack".to_string(),
            version: PackVersion {
                id: id.to_string(),
                name: version.to_string(),
                ..Default::default()
            },
            root: PathBuf::from("/staging/fixture-pack"),
            files,
            requirements: None,
            notices: Vec::new(),
        }
    }

    /// A staged file whose fingerprint matches `data`, for live comparisons.
    fn staged_bytes(rel: &str, data: &[u8]) -> PreparedFile {
        PreparedFile {
            rel_path: PathBuf::from(rel),
            size: data.len() as u64,
            sha256: sha256_bytes(data),
        }
    }

    /// An overlay file whose fingerprint matches `data`.
    fn overlay_bytes(rel: &str, data: &[u8]) -> OverlayFile {
        OverlayFile {
            rel_path: PathBuf::from(rel),
            source: PathBuf::from(format!("/overlay/{rel}")),
            sha256: sha256_bytes(data),
            size: data.len() as u64,
        }
    }

    fn fingerprint(data: &[u8]) -> ManagedFile {
        ManagedFile::new(sha256_bytes(data), data.len() as u64)
    }

    fn rels(changes: &[FileChange]) -> Vec<PathBuf> {
        changes.iter().map(|c| c.rel_path.clone()).collect()
    }

    fn overlay_rels(changes: &[OverlayChange]) -> Vec<PathBuf> {
        changes.iter().map(|c| c.rel_path.clone()).collect()
    }

    fn plan_paths(plan: &UpdatePlan) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = plan
            .additions
            .iter()
            .chain(plan.modifications.iter())
            .chain(plan.removals.iter())
            .map(|c| c.rel_path.clone())
            .collect();
        paths.extend(plan.overlay_changes.iter().map(|c| c.rel_path.clone()));
        paths
    }

    /// The design notes "Example Expected Behavior" scenario.
    #[test]
    fn example_expected_behavior_scenario() {
        let mut from = InstalledState {
            installed_version: Some("4.11".to_string()),
            provider_version_id: Some("provider-11".to_string()),
            ..InstalledState::default()
        };
        from.managed_files.insert(
            "mods/upstream-a.jar".to_string(),
            managed("a-installed", 10),
        );
        from.managed_files.insert(
            "mods/upstream-old.jar".to_string(),
            managed("old-installed", 10),
        );
        from.managed_files.insert(
            "config/MiniMOTD/main.conf".to_string(),
            managed("installed-main", 10),
        );

        let desired = pack(
            "4.12",
            "provider-12",
            vec![
                staged("mods/upstream-a-new.jar", "a-new"),
                staged("mods/upstream-b.jar", "b"),
                staged("config/MiniMOTD/main.conf", "upstream-main"),
            ],
        );
        let overlay = vec![
            overlay_file("mods/grieflogger.jar", "grieflogger"),
            overlay_file("config/MiniMOTD/main.conf", "overlay-main"),
        ];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &overlay)
            .unwrap();

        assert_eq!(
            rels(&plan.removals),
            vec![
                PathBuf::from("mods/upstream-a.jar"),
                PathBuf::from("mods/upstream-old.jar"),
            ]
        );
        assert_eq!(
            rels(&plan.additions),
            vec![
                PathBuf::from("mods/upstream-a-new.jar"),
                PathBuf::from("mods/upstream-b.jar"),
            ]
        );

        assert!(
            !rels(&plan.additions).contains(&PathBuf::from("config/MiniMOTD/main.conf")),
            "overlay-handled upstream config must not be added"
        );
        assert!(
            !rels(&plan.modifications).contains(&PathBuf::from("config/MiniMOTD/main.conf")),
            "overlay-handled upstream config must not be replaced from upstream"
        );

        let main_conf = plan
            .overlay_changes
            .iter()
            .find(|c| c.rel_path == Path::new("config/MiniMOTD/main.conf"))
            .expect("overlay change for main.conf");
        assert_eq!(main_conf.status, OverlayChangeStatus::ReplacesChanged);

        let grieflogger = plan
            .overlay_changes
            .iter()
            .find(|c| c.rel_path == Path::new("mods/grieflogger.jar"))
            .expect("overlay change for grieflogger.jar");
        assert_eq!(grieflogger.status, OverlayChangeStatus::Applied);

        let notice = plan
            .notices
            .iter()
            .find(|n| n.path.as_deref() == Some(Path::new("config/MiniMOTD/main.conf")))
            .expect("conflict notice for main.conf");
        assert!(
            notice.message.contains("Upstream changed this file.")
                && notice.message.contains("Local overlay replaces it.")
                && notice.message.contains("Overlay version will be used.")
        );

        assert_eq!(plan.from_version.as_deref(), Some("4.11"));
        assert_eq!(plan.from_id.as_deref(), Some("provider-11"));
        assert_eq!(plan.to_version, "4.12");
        assert_eq!(plan.to_id, "provider-12");

        assert!(
            !plan_paths(&plan).iter().any(|p| p.starts_with("world")),
            "world data must never be referenced by the plan"
        );
    }

    #[test]
    fn new_modified_and_removed_upstream_files() {
        let mut from = InstalledState::default();
        from.managed_files
            .insert("mods/removed.jar".to_string(), managed("removed-old", 10));
        from.managed_files
            .insert("mods/changed.jar".to_string(), managed("changed-v1", 10));

        let desired = pack(
            "2",
            "id-2",
            vec![
                staged("mods/new.jar", "new-content"),
                staged("mods/changed.jar", "changed-v2"),
            ],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &[])
            .unwrap();

        assert_eq!(rels(&plan.additions), vec![PathBuf::from("mods/new.jar")]);
        assert_eq!(
            rels(&plan.modifications),
            vec![PathBuf::from("mods/changed.jar")]
        );
        assert_eq!(
            rels(&plan.removals),
            vec![PathBuf::from("mods/removed.jar")]
        );
    }

    #[test]
    fn unknown_server_file_is_not_in_the_plan() {
        let from = InstalledState::default();
        let desired = pack(
            "1",
            "id-1",
            vec![staged("mods/a.jar", "a"), staged("config/b.toml", "b")],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &[])
            .unwrap();

        let paths = plan_paths(&plan);
        assert!(
            !paths.contains(&PathBuf::from("local-only/custom.jar")),
            "an on-disk unknown file must have no plan entry and therefore survives"
        );
        assert!(!paths.contains(&PathBuf::from("world/session.lock")));
    }

    #[test]
    fn persistent_data_survives() {
        let mut from = InstalledState::default();
        from.managed_files
            .insert("world/data.dat".to_string(), managed("world-old", 10));
        from.managed_files
            .insert("ops.json".to_string(), managed("ops-old", 10));

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged("world/data.dat", "world-new"),
                staged("ops.json", "ops-new"),
                staged("mods/real.jar", "real"),
            ],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &[])
            .unwrap();

        let paths = plan_paths(&plan);
        assert!(
            !paths.contains(&PathBuf::from("world/data.dat")),
            "persistent world data must produce no plan entry even when upstream changed"
        );
        assert!(
            !paths.contains(&PathBuf::from("ops.json")),
            "persistent ops.json must produce no plan entry even when upstream changed"
        );
        assert_eq!(rels(&plan.additions), vec![PathBuf::from("mods/real.jar")]);
    }

    #[test]
    fn overlay_adds_a_brand_new_file() {
        let from = InstalledState::default();
        let desired = pack("1", "id-1", vec![staged("mods/a.jar", "a")]);
        let overlay = vec![overlay_file("mods/custom.jar", "custom")];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &overlay)
            .unwrap();

        let custom = plan
            .overlay_changes
            .iter()
            .find(|c| c.rel_path == Path::new("mods/custom.jar"))
            .expect("overlay change for custom.jar");
        assert_eq!(custom.status, OverlayChangeStatus::Applied);
        assert!(!rels(&plan.additions).contains(&PathBuf::from("mods/custom.jar")));
        assert!(!rels(&plan.modifications).contains(&PathBuf::from("mods/custom.jar")));
        assert!(!rels(&plan.removals).contains(&PathBuf::from("mods/custom.jar")));
    }

    #[test]
    fn overlay_replaces_unchanged_upstream_file() {
        let mut from = InstalledState::default();
        from.managed_files.insert(
            "config/example.toml".to_string(),
            managed("same-content", 10),
        );

        let desired = pack(
            "1",
            "id-1",
            vec![staged("config/example.toml", "same-content")],
        );
        let overlay = vec![overlay_file("config/example.toml", "overlay-content")];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &overlay)
            .unwrap();

        let change = plan
            .overlay_changes
            .iter()
            .find(|c| c.rel_path == Path::new("config/example.toml"))
            .expect("overlay change for example.toml");
        assert_eq!(change.status, OverlayChangeStatus::ReplacesUnchanged);
        assert!(
            plan.notices.is_empty(),
            "no conflict notice when upstream did not change"
        );
        assert!(!rels(&plan.modifications).contains(&PathBuf::from("config/example.toml")));
    }

    #[test]
    fn overlay_replaces_changed_upstream_file_with_notice() {
        let mut from = InstalledState::default();
        from.managed_files.insert(
            "config/example.toml".to_string(),
            managed("old-content", 10),
        );

        let desired = pack(
            "2",
            "id-2",
            vec![staged("config/example.toml", "new-content")],
        );
        let overlay = vec![overlay_file("config/example.toml", "overlay-content")];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &overlay)
            .unwrap();

        let change = plan
            .overlay_changes
            .iter()
            .find(|c| c.rel_path == Path::new("config/example.toml"))
            .expect("overlay change for example.toml");
        assert_eq!(change.status, OverlayChangeStatus::ReplacesChanged);

        let notice = plan
            .notices
            .iter()
            .find(|n| n.path.as_deref() == Some(Path::new("config/example.toml")))
            .expect("conflict notice");
        assert_eq!(
            notice.message,
            CONFLICT_NOTICE.replace("{path}", "config/example.toml")
        );
    }

    #[test]
    fn empty_update_when_from_equals_desired() {
        let mut from = InstalledState {
            installed_version: Some("1".to_string()),
            provider_version_id: Some("id-1".to_string()),
            ..InstalledState::default()
        };
        from.managed_files
            .insert("mods/a.jar".to_string(), managed("a-content", 10));
        from.managed_files
            .insert("config/b.toml".to_string(), managed("b-content", 10));

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged("mods/a.jar", "a-content"),
                staged("config/b.toml", "b-content"),
            ],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &[])
            .unwrap();

        assert!(plan.is_empty());
        assert_eq!(plan.upstream_change_count(), 0);
    }

    #[test]
    fn live_plan_replaces_missing_or_tampered_managed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(root.join("mods")).unwrap();
        std::fs::write(root.join("mods/a.jar"), b"tampered").unwrap();
        let mut from = InstalledState::default();
        from.managed_files
            .insert("mods/a.jar".to_string(), managed("expected", 8));
        let desired = pack("1", "id-1", vec![staged("mods/a.jar", "expected")]);
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &[], &root)
            .unwrap();
        assert_eq!(rels(&plan.modifications), vec![PathBuf::from("mods/a.jar")]);
        std::fs::remove_file(root.join("mods/a.jar")).unwrap();
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &[], &root)
            .unwrap();
        assert_eq!(rels(&plan.modifications), vec![PathBuf::from("mods/a.jar")]);
    }

    #[test]
    fn upstream_change_count_sums_correctly() {
        let mut from = InstalledState::default();
        from.managed_files
            .insert("mods/removed.jar".to_string(), managed("r-old", 10));
        from.managed_files
            .insert("mods/changed.jar".to_string(), managed("c-old", 10));

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged("mods/new1.jar", "n1"),
                staged("mods/new2.jar", "n2"),
                staged("mods/changed.jar", "c-new"),
            ],
        );
        let overlay = vec![overlay_file("mods/custom.jar", "custom")];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &overlay)
            .unwrap();

        assert_eq!(plan.additions.len(), 2);
        assert_eq!(plan.modifications.len(), 1);
        assert_eq!(plan.removals.len(), 1);
        assert_eq!(plan.upstream_change_count(), 4);
        assert!(!plan.is_empty());
    }

    #[test]
    fn server_properties_from_overlay_is_applied() {
        let from = InstalledState::default();

        let desired = pack(
            "1",
            "id-1",
            vec![staged("server.properties", "upstream-props")],
        );
        let overlay = vec![overlay_file("server.properties", "overlay-props")];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &overlay)
            .unwrap();

        let change = plan
            .overlay_changes
            .iter()
            .find(|c| c.rel_path == Path::new("server.properties"))
            .expect("server.properties overlay change");
        assert_eq!(change.status, OverlayChangeStatus::Applied);
        assert!(
            !rels(&plan.additions).contains(&PathBuf::from("server.properties")),
            "persistent-by-default is overridden by explicit overlay presence"
        );
    }

    #[test]
    fn deterministic_sorting_of_scrambled_inputs() {
        let mut from = InstalledState::default();
        from.managed_files
            .insert("mods/old-z.jar".to_string(), managed("oz-old", 10));
        from.managed_files
            .insert("mods/old-a.jar".to_string(), managed("oa-old", 10));
        from.managed_files
            .insert("mods/m.jar".to_string(), managed("m-old", 10));

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged("mods/a.jar", "a-new"),
                staged("mods/z.jar", "z-new"),
                staged("mods/m.jar", "m-new"),
                staged("config/b.toml", "b-new"),
            ],
        );
        let overlay = vec![
            overlay_file("mods/y.jar", "y-overlay"),
            overlay_file("config/a.toml", "a-overlay"),
        ];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&from, &desired, &overlay)
            .unwrap();

        assert_eq!(
            rels(&plan.additions),
            vec![
                PathBuf::from("config/b.toml"),
                PathBuf::from("mods/a.jar"),
                PathBuf::from("mods/z.jar"),
            ]
        );
        assert_eq!(rels(&plan.modifications), vec![PathBuf::from("mods/m.jar")]);
        assert_eq!(
            rels(&plan.removals),
            vec![
                PathBuf::from("mods/old-a.jar"),
                PathBuf::from("mods/old-z.jar"),
            ]
        );
        assert_eq!(
            overlay_rels(&plan.overlay_changes),
            vec![PathBuf::from("config/a.toml"), PathBuf::from("mods/y.jar"),]
        );
    }

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, data).unwrap();
    }

    #[test]
    fn sync_sweeps_stale_files_under_pack_folders_even_without_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        // A server that packctl has never touched.
        write(&root, "mods/stale-old.jar", b"old");
        write(&root, "mods/new.jar", b"hand-installed");

        let desired = pack(
            "2",
            "id-2",
            vec![
                staged_bytes("mods/new.jar", b"new"),
                staged_bytes("config/x.toml", b"x"),
            ],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        assert_eq!(
            rels(&plan.removals),
            vec![PathBuf::from("mods/stale-old.jar")]
        );
        assert_eq!(
            plan.unmanaged_removals,
            vec![PathBuf::from("mods/stale-old.jar")],
            "packctl never installed the stale file"
        );
        assert!(plan.needs_force_sync());
        assert_eq!(rels(&plan.additions), vec![PathBuf::from("config/x.toml")]);
        assert_eq!(
            rels(&plan.modifications),
            vec![PathBuf::from("mods/new.jar")]
        );
    }

    #[test]
    fn first_update_skips_identical_live_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "mods/a.jar", b"jar-a");
        write(&root, "config/b.toml", b"conf-b");

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged_bytes("mods/a.jar", b"jar-a"),
                staged_bytes("config/b.toml", b"conf-b"),
            ],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        assert!(plan.additions.is_empty(), "{:?}", plan.additions);
        assert!(plan.modifications.is_empty(), "{:?}", plan.modifications);
        assert!(plan.is_empty());
        assert!(!plan.needs_force_sync());
        // Nothing to write, but the version still has to be recorded.
        assert!(plan.needs_state_commit(&InstalledState::default(), &InstalledState::default()));
    }

    #[test]
    fn first_update_differing_unmanaged_file_is_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "config/b.toml", b"edited by hand");

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged_bytes("config/b.toml", b"pack default"),
                staged_bytes("mods/a.jar", b"jar-a"),
            ],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        assert_eq!(
            rels(&plan.modifications),
            vec![PathBuf::from("config/b.toml")]
        );
        assert_eq!(plan.modifications[0].kind, ChangeKind::Replace);
        assert_eq!(rels(&plan.additions), vec![PathBuf::from("mods/a.jar")]);
    }

    /// The state a recorded v1 leaves: `mods/a.jar` installed by packctl.
    fn recorded_v1() -> InstalledState {
        let mut from = InstalledState {
            installed_version: Some("1".to_string()),
            provider_version_id: Some("id-1".to_string()),
            ..InstalledState::default()
        };
        from.managed_files
            .insert("mods/a.jar".to_string(), fingerprint(b"jar-a"));
        from
    }

    #[test]
    fn managed_server_unrecorded_differing_file_needs_force_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "mods/a.jar", b"jar-a");
        // The admin added mod Y by hand and tuned the config it generated.
        write(&root, "config/y-common.toml", b"tuned by the admin");
        // A file the pack ships identically, or that is missing, is fine.
        write(&root, "config/same.toml", b"same");

        let desired = pack(
            "2",
            "id-2",
            vec![
                staged_bytes("mods/a.jar", b"jar-a"),
                staged_bytes("config/y-common.toml", b"pack default"),
                staged_bytes("config/same.toml", b"same"),
                staged_bytes("config/new.toml", b"new"),
            ],
        );
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&recorded_v1(), &desired, &[], &root)
            .unwrap();

        assert_eq!(
            rels(&plan.modifications),
            vec![PathBuf::from("config/y-common.toml")]
        );
        assert_eq!(
            plan.unmanaged_overwrites,
            vec![PathBuf::from("config/y-common.toml")]
        );
        assert!(plan.needs_force_sync());

        // A server with no state is the first-update guard's job instead.
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();
        assert!(plan.unmanaged_overwrites.is_empty());
        assert!(!plan.needs_force_sync());
    }

    #[test]
    fn overwriting_a_file_packctl_wrote_does_not_need_force_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "mods/a.jar", b"jar-a");
        write(&root, "config/x.toml", b"once from the overlay");
        let mut from = recorded_v1();
        // The overlay used to provide config/x.toml; the admin removed it
        // from the overlay and the pack now ships the path.
        from.overlay_applied.insert(
            "config/x.toml".to_string(),
            fingerprint(b"once from the overlay"),
        );

        let desired = pack(
            "2",
            "id-2",
            vec![
                staged_bytes("mods/a.jar", b"jar-a"),
                staged_bytes("config/x.toml", b"pack"),
            ],
        );
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &[], &root)
            .unwrap();

        assert_eq!(
            rels(&plan.modifications),
            vec![PathBuf::from("config/x.toml")]
        );
        assert!(plan.unmanaged_overwrites.is_empty());
        assert!(!plan.needs_force_sync());
    }

    #[test]
    fn recorded_files_in_a_folder_the_new_version_drops_are_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "mods/a.jar", b"jar-a");
        write(&root, "kubejs/startup.js", b"v1 script");
        // Recorded by v0.1.0, which did not strip a wrapper folder.
        write(&root, "Wrap-1.0/mods/w.jar", b"w");
        // Not recorded: never touched outside the folders the pack ships.
        write(&root, "kubejs/admin.js", b"admin");
        let mut from = recorded_v1();
        from.managed_files
            .insert("kubejs/startup.js".to_string(), fingerprint(b"v1 script"));
        from.managed_files
            .insert("Wrap-1.0/mods/w.jar".to_string(), fingerprint(b"w"));
        // Recorded but already gone, and a key that is not a safe path.
        from.managed_files
            .insert("kubejs/gone.js".to_string(), fingerprint(b"gone"));
        from.managed_files
            .insert("../outside.jar".to_string(), fingerprint(b"x"));

        let desired = pack("2", "id-2", vec![staged_bytes("mods/a.jar", b"jar-a")]);
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &[], &root)
            .unwrap();

        assert_eq!(
            rels(&plan.removals),
            vec![
                PathBuf::from("Wrap-1.0/mods/w.jar"),
                PathBuf::from("kubejs/startup.js")
            ]
        );
        assert!(plan.unmanaged_removals.is_empty());
        assert!(!plan.needs_force_sync());
    }

    #[test]
    fn unrecorded_directory_in_the_way_is_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(root.join("mods/a.jar")).unwrap();

        let desired = pack("1", "id-1", vec![staged_bytes("mods/a.jar", b"jar")]);
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        assert_eq!(rels(&plan.modifications), vec![PathBuf::from("mods/a.jar")]);
    }

    #[test]
    fn overlay_already_applied_is_not_pending_and_plan_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "mods/a.jar", b"jar-a");
        write(&root, "config/b.toml", b"overlay-b");
        write(&root, "mods/extra.jar", b"extra");

        let mut from = InstalledState {
            installed_version: Some("1".to_string()),
            provider_version_id: Some("id-1".to_string()),
            ..InstalledState::default()
        };
        from.managed_files
            .insert("mods/a.jar".to_string(), fingerprint(b"jar-a"));
        from.overlay_shadowed
            .insert("config/b.toml".to_string(), fingerprint(b"pack-b"));
        from.overlay_applied
            .insert("config/b.toml".to_string(), fingerprint(b"overlay-b"));
        from.overlay_applied
            .insert("mods/extra.jar".to_string(), fingerprint(b"extra"));

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged_bytes("mods/a.jar", b"jar-a"),
                staged_bytes("config/b.toml", b"pack-b"),
            ],
        );
        let overlay = vec![
            overlay_bytes("config/b.toml", b"overlay-b"),
            overlay_bytes("mods/extra.jar", b"extra"),
        ];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &overlay, &root)
            .unwrap();

        assert_eq!(plan.overlay_changes.len(), 2);
        assert!(plan.overlay_changes.iter().all(|c| !c.pending));
        assert!(plan.overlay_changes.iter().all(|c| !c.live_drift));
        assert_eq!(plan.pending_overlay_count(), 0);
        assert!(plan.is_empty());
        let b = plan
            .overlay_changes
            .iter()
            .find(|c| c.rel_path == Path::new("config/b.toml"))
            .unwrap();
        assert_eq!(b.status, OverlayChangeStatus::ReplacesUnchanged);
    }

    #[test]
    fn overlay_pending_when_live_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        // The admin edited the overlay file; the live file still holds the
        // content packctl wrote last time.
        write(&root, "config/b.toml", b"overlay-v1");

        let mut from = InstalledState {
            installed_version: Some("1".to_string()),
            provider_version_id: Some("id-1".to_string()),
            ..InstalledState::default()
        };
        from.overlay_applied
            .insert("config/b.toml".to_string(), fingerprint(b"overlay-v1"));
        let desired = pack("1", "id-1", vec![]);
        let overlay = vec![
            overlay_bytes("config/b.toml", b"overlay-v2"),
            overlay_bytes("config/new.toml", b"new"),
        ];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &overlay, &root)
            .unwrap();

        assert!(plan.overlay_changes.iter().all(|c| c.pending));
        assert!(plan.overlay_changes.iter().all(|c| !c.live_drift));
        assert_eq!(plan.pending_overlay_count(), 2);
        assert!(!plan.is_empty());
        assert!(!plan.needs_force_sync());
    }

    #[test]
    fn overlay_live_drift_is_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        // The server rewrote the file after packctl applied the overlay.
        write(&root, "config/quests.snbt", b"edited in game");

        let mut from = InstalledState {
            installed_version: Some("1".to_string()),
            provider_version_id: Some("id-1".to_string()),
            ..InstalledState::default()
        };
        from.overlay_applied
            .insert("config/quests.snbt".to_string(), fingerprint(b"adopted"));
        let desired = pack("1", "id-1", vec![]);
        let overlay = vec![overlay_bytes("config/quests.snbt", b"adopted")];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &overlay, &root)
            .unwrap();

        assert!(plan.overlay_changes[0].pending);
        assert!(plan.overlay_changes[0].live_drift);
        assert_eq!(
            plan.live_drift_paths(),
            vec![Path::new("config/quests.snbt")]
        );
        assert!(plan.needs_force_sync());
    }

    #[test]
    fn overlay_over_differing_file_on_unrecorded_server_is_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "server.properties", b"live");

        let desired = pack("1", "id-1", vec![]);
        let overlay = vec![overlay_bytes("server.properties", b"overlay")];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &overlay, &root)
            .unwrap();

        assert!(plan.overlay_changes[0].live_drift);

        // Once packctl has a record of the server, a new overlay file simply
        // wins over the live file.
        let from = InstalledState {
            installed_version: Some("1".to_string()),
            ..InstalledState::default()
        };
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &overlay, &root)
            .unwrap();
        assert!(plan.overlay_changes[0].pending);
        assert!(!plan.overlay_changes[0].live_drift);
    }

    #[test]
    fn overlay_shadowed_detects_upstream_change_across_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "config/b.toml", b"overlay");
        let overlay = vec![overlay_bytes("config/b.toml", b"overlay")];

        // After the v1 update the overlay owns the path; state remembers the
        // v1 upstream content as shadowed, not managed.
        let mut from = InstalledState {
            installed_version: Some("1".to_string()),
            provider_version_id: Some("id-1".to_string()),
            ..InstalledState::default()
        };
        from.overlay_shadowed
            .insert("config/b.toml".to_string(), fingerprint(b"upstream-v1"));
        from.overlay_applied
            .insert("config/b.toml".to_string(), fingerprint(b"overlay"));

        let v2 = pack(
            "2",
            "id-2",
            vec![staged_bytes("config/b.toml", b"upstream-v2")],
        );
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &v2, &overlay, &root)
            .unwrap();
        assert_eq!(
            plan.overlay_changes[0].status,
            OverlayChangeStatus::ReplacesChanged
        );
        assert!(
            plan.notices
                .iter()
                .any(|n| n.path.as_deref() == Some(Path::new("config/b.toml")))
        );

        // The v2 update records the v2 upstream content as shadowed; when v3
        // changes the file again, the conflict is raised again.
        from.installed_version = Some("2".to_string());
        from.provider_version_id = Some("id-2".to_string());
        from.overlay_shadowed
            .insert("config/b.toml".to_string(), fingerprint(b"upstream-v2"));
        let v3 = pack(
            "3",
            "id-3",
            vec![staged_bytes("config/b.toml", b"upstream-v3")],
        );
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &v3, &overlay, &root)
            .unwrap();
        assert_eq!(
            plan.overlay_changes[0].status,
            OverlayChangeStatus::ReplacesChanged
        );
        assert!(!plan.overlay_changes[0].pending, "overlay already in place");

        // Re-planning v2 itself raises nothing.
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &v2, &overlay, &root)
            .unwrap();
        assert_eq!(
            plan.overlay_changes[0].status,
            OverlayChangeStatus::ReplacesUnchanged
        );
        assert!(plan.notices.is_empty());
    }

    #[test]
    fn removals_of_recorded_files_are_not_unmanaged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "mods/old.jar", b"old");
        write(&root, "mods/was-overlay.jar", b"o");
        write(&root, "mods/dropped-in.jar", b"admin");

        let mut from = InstalledState {
            installed_version: Some("1".to_string()),
            ..InstalledState::default()
        };
        from.managed_files
            .insert("mods/old.jar".to_string(), fingerprint(b"old"));
        from.overlay_applied
            .insert("mods/was-overlay.jar".to_string(), fingerprint(b"o"));

        let desired = pack("2", "id-2", vec![staged_bytes("mods/new.jar", b"new")]);
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&from, &desired, &[], &root)
            .unwrap();

        assert_eq!(plan.removals.len(), 3);
        assert_eq!(
            plan.unmanaged_removals,
            vec![PathBuf::from("mods/dropped-in.jar")]
        );
    }

    #[test]
    fn pack_files_in_reserved_locations_are_skipped_with_a_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged_bytes(".packctl.toml", b"[controller]"),
                staged_bytes(".packctlignore", b"*"),
                staged_bytes(".packctl/snapshots/x/manifest.json", b"{}"),
                staged_bytes("overlay/mods/evil.jar", b"evil"),
                staged_bytes("packs/evil.zip", b"zip"),
                staged_bytes("mods/a.jar", b"a"),
            ],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        assert_eq!(rels(&plan.additions), vec![PathBuf::from("mods/a.jar")]);
        let notice = plan
            .notices
            .iter()
            .find(|n| n.message.starts_with("Skipped 5 pack file(s)"))
            .expect("reserved notice");
        assert!(notice.message.contains(".packctl.toml"));
        assert!(notice.message.contains("overlay/mods/evil.jar"));
    }

    #[test]
    fn custom_reserved_paths_protect_nested_overlay_from_sweep_and_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "config/my-overlay/mods/x.jar", b"overlay");
        write(&root, "config/stale.toml", b"stale");

        let reserved = {
            use crate::config::profile::{
                ControllerKind, ControllerSection, OverlaySection, PackSection, SecretsSection,
                ServerProfile, ServerSection,
            };
            ReservedPaths::for_profile(&ServerProfile {
                name: "t".to_string(),
                server: ServerSection { root: root.clone() },
                pack: PackSection::curseforge(1),
                overlay: OverlaySection {
                    path: root.join("config").join("my-overlay"),
                },
                controller: ControllerSection {
                    kind: ControllerKind::Command,
                    instance: None,
                    command: None,
                },
                secrets: SecretsSection::default(),
            })
        };

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged_bytes("config/a.toml", b"a"),
                staged_bytes("config/my-overlay/mods/y.jar", b"y"),
            ],
        );
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .with_reserved(reserved)
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        assert_eq!(
            rels(&plan.removals),
            vec![PathBuf::from("config/stale.toml")]
        );
        assert_eq!(rels(&plan.additions), vec![PathBuf::from("config/a.toml")]);
    }

    #[test]
    fn prepared_notices_become_plan_notices() {
        let mut desired = pack("1", "id-1", vec![]);
        desired.notices.push("Archive wraps the server".to_string());
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&InstalledState::default(), &desired, &[])
            .unwrap();
        assert_eq!(plan.notices.len(), 1);
        assert_eq!(plan.notices[0].path, None);
        assert!(plan.is_empty(), "notices never make a plan non-empty");
    }

    #[test]
    fn needs_state_commit_compares_version_and_tracked_files() {
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&InstalledState::default(), &pack("1", "id-1", vec![]), &[])
            .unwrap();
        let recorded = InstalledState {
            installed_version: Some("1".to_string()),
            provider_version_id: Some("id-1".to_string()),
            ..InstalledState::default()
        };
        assert!(plan.needs_state_commit(&InstalledState::default(), &recorded));
        assert!(!plan.needs_state_commit(&recorded, &recorded));

        let mut next = recorded.clone();
        next.overlay_shadowed
            .insert("config/a.toml".to_string(), fingerprint(b"a"));
        assert!(plan.needs_state_commit(&recorded, &next));
    }

    #[test]
    fn needs_state_commit_records_a_newly_known_minecraft_version() {
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan(&InstalledState::default(), &pack("1", "id-1", vec![]), &[])
            .unwrap();
        // State written before the Minecraft version was recorded.
        let old = InstalledState {
            installed_version: Some("1".to_string()),
            provider_version_id: Some("id-1".to_string()),
            ..InstalledState::default()
        };
        let next = InstalledState {
            minecraft: Some("1.21.1".to_string()),
            ..old.clone()
        };
        assert!(plan.needs_state_commit(&old, &next));
        assert!(!plan.needs_state_commit(&next, &next));
        // An unknown target Minecraft version never forces a commit.
        assert!(!plan.needs_state_commit(&next, &old));
    }

    #[test]
    fn sync_never_touches_files_outside_pack_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "libraries/net/lib.jar", b"runtime");
        write(&root, "datapacks/custom/dp.json", b"data");
        write(&root, "custom.txt", b"top level");

        let desired = pack("1", "id-1", vec![staged("mods/a.jar", "a")]);

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        let paths = plan_paths(&plan);
        assert!(!paths.contains(&PathBuf::from("libraries/net/lib.jar")));
        assert!(!paths.contains(&PathBuf::from("datapacks/custom/dp.json")));
        assert!(!paths.contains(&PathBuf::from("custom.txt")));
    }

    #[test]
    fn sync_keeps_overlay_files_under_pack_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "mods/stale.jar", b"stale");
        write(&root, "mods/custom.jar", b"overlay");

        let desired = pack("1", "id-1", vec![staged("mods/a.jar", "a")]);
        let overlay = vec![overlay_file("mods/custom.jar", "overlay")];

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &overlay, &root)
            .unwrap();

        let removals = rels(&plan.removals);
        assert!(
            !removals.contains(&PathBuf::from("mods/custom.jar")),
            "overlay file must not be swept"
        );
        assert!(removals.contains(&PathBuf::from("mods/stale.jar")));
    }

    #[test]
    fn sync_keeps_persistent_paths_even_under_pack_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        // A pack that ships world/ must never wipe the live world.
        write(&root, "world/region/r.mca", b"live world");
        write(&root, "world/level.dat", b"live level");
        write(&root, "world/region/old-backup.mca", b"stale backup");

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged("world/level.dat", "pack-level"),
                staged("mods/a.jar", "a"),
            ],
        );

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        let paths = plan_paths(&plan);
        assert!(
            !paths.iter().any(|p| p.starts_with("world")),
            "world must never be planned"
        );
        assert_eq!(rels(&plan.additions), vec![PathBuf::from("mods/a.jar")]);
    }

    #[test]
    fn sync_never_sweeps_reserved_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "packs/archive.zip", b"zip");
        write(&root, "overlay/mods/grieflogger.jar", b"gr");
        write(&root, ".packctl/state.json", b"{}");

        let desired = pack("1", "id-1", vec![staged("mods/a.jar", "a")]);

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        let paths = plan_paths(&plan);
        assert!(!paths.contains(&PathBuf::from("packs/archive.zip")));
        assert!(!paths.contains(&PathBuf::from("overlay/mods/grieflogger.jar")));
        assert!(!paths.contains(&PathBuf::from(".packctl/state.json")));
    }

    #[test]
    fn sync_treats_missing_pack_folders_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();

        let desired = pack("1", "id-1", vec![staged("mods/a.jar", "a")]);

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        assert_eq!(rels(&plan.removals), Vec::<PathBuf>::new());
        assert_eq!(rels(&plan.additions), vec![PathBuf::from("mods/a.jar")]);
    }

    #[test]
    fn ignored_paths_are_never_swept_or_updated() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "config/luckperms/luckperms-h2-v2.mv.db", b"data");
        write(&root, "config/luckperms/luckperms.conf", b"tuned");
        write(&root, "config/jei-server.toml", b"runtime");
        write(&root, "config/mod.toml", b"pack-shipped");

        let desired = pack(
            "1",
            "id-1",
            vec![
                staged("config/mod.toml", "pack-new"),
                staged("config/luckperms/luckperms.conf", "pack-version"),
            ],
        );
        let ignore = parse_rules("config/luckperms\nconfig/jei-server.toml\n").unwrap();

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .with_ignore(ignore)
            .build_plan_for_server(&InstalledState::default(), &desired, &[], &root)
            .unwrap();

        let paths = plan_paths(&plan);
        assert!(!paths.contains(&PathBuf::from("config/luckperms/luckperms-h2-v2.mv.db")));
        assert!(!paths.contains(&PathBuf::from("config/luckperms/luckperms.conf")));
        assert!(!paths.contains(&PathBuf::from("config/jei-server.toml")));
        assert!(paths.contains(&PathBuf::from("config/mod.toml")));
    }

    #[test]
    fn overlay_still_applies_to_ignored_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        write(&root, "config/luckperms/luckperms.conf", b"runtime");
        write(&root, "config/mod.toml", b"pack");

        let desired = pack("1", "id-1", vec![staged("config/mod.toml", "pack-new")]);
        let overlay = vec![overlay_file("config/luckperms/luckperms.conf", "overlay")];
        let ignore = parse_rules("config/luckperms\n").unwrap();

        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .with_ignore(ignore)
            .build_plan_for_server(&InstalledState::default(), &desired, &overlay, &root)
            .unwrap();

        assert!(
            plan.overlay_changes
                .iter()
                .any(|c| c.rel_path == Path::new("config/luckperms/luckperms.conf")),
            "overlay content must still apply even when the path is ignored"
        );
        assert!(!rels(&plan.removals).contains(&PathBuf::from("config/luckperms/luckperms.conf")));
    }
}
