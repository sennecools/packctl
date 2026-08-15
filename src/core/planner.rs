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
//! The public API in this module is consumed by the executor and CLI modules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::overlay::OverlayFile;
use crate::core::ownership::FilePolicy;
use crate::core::state::InstalledState;
use crate::error::Result;
use crate::fs::hashing::sha256_file;
use crate::fs::paths::safe_join;
use crate::providers::{PreparedFile, PreparedPack};

/// What should happen to a managed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// The file is new to the upstream version and must be written.
    Add,
    /// An existing managed file must be overwritten with new content.
    Replace,
    /// A previously managed file disappeared upstream and must be removed.
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
    /// The overlay replaces a previously managed file whose upstream content
    /// did not change; the overlay is simply re-applied.
    ReplacesUnchanged,
    /// The overlay replaces a previously managed file whose upstream content
    /// changed; surfaced as an informational conflict.
    ReplacesChanged,
}

/// One overlay file change in an [`UpdatePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayChange {
    /// Destination path relative to the server root (forward slashes).
    pub rel_path: PathBuf,
    pub status: OverlayChangeStatus,
}

/// An informational note attached to a plan (never an error in V1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNotice {
    /// The path the notice is about, when it concerns a specific file.
    pub path: Option<PathBuf>,
    pub message: String,
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
}

impl UpdatePlan {
    /// True when there is no work to perform at all.
    ///
    /// Notices alone (informational) never make a plan non-empty.
    pub fn is_empty(&self) -> bool {
        self.additions.is_empty()
            && self.modifications.is_empty()
            && self.removals.is_empty()
            && self.overlay_changes.is_empty()
    }

    /// How many upstream (non-overlay) file changes the plan contains.
    pub fn upstream_change_count(&self) -> usize {
        self.additions.len() + self.modifications.len() + self.removals.len()
    }
}

/// Builds [`UpdatePlan`]s from installed state, prepared upstream, and overlay.
pub struct UpdatePlanner {
    pub policy: FilePolicy,
}

impl UpdatePlanner {
    pub fn new(policy: FilePolicy) -> Self {
        UpdatePlanner { policy }
    }

    /// Compares `from` (what the updater previously managed) against `desired`
    /// (the prepared upstream version, authoritative for pack-managed content)
    /// and `overlay` (the mirrored local overlay, which always wins).
    ///
    /// Pure computation: never reads or mutates the live server.
    pub fn build_plan(
        &self,
        from: &InstalledState,
        desired: &PreparedPack,
        overlay: &[OverlayFile],
    ) -> Result<UpdatePlan> {
        self.build_plan_with_live_root(from, desired, overlay, None)
    }

    /// Builds a plan while checking that files recorded as managed still match
    /// their persisted fingerprints. Missing or modified files are replaced
    /// even when the desired upstream fingerprint is unchanged.
    pub fn build_plan_for_server(
        &self,
        from: &InstalledState,
        desired: &PreparedPack,
        overlay: &[OverlayFile],
        server_root: &Path,
    ) -> Result<UpdatePlan> {
        self.build_plan_with_live_root(from, desired, overlay, Some(server_root))
    }

    fn build_plan_with_live_root(
        &self,
        from: &InstalledState,
        desired: &PreparedPack,
        overlay: &[OverlayFile],
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

        // Rule 1: removals.
        //
        // A managed file may only be removed when it is no longer provided by
        // the upstream version, is not protected by persistent-data policy,
        // and is not provided by the overlay (overlay wins, so it stays on
        // disk). Unknown/untracked files never enter the plan and survive.
        for path in from.managed_files.keys() {
            let rel = Path::new(path);
            if self.policy.is_persistent(rel) {
                continue;
            }
            if overlay_files.contains_key(path) {
                continue;
            }
            if !desired_files.contains_key(path) {
                removals.push(FileChange {
                    rel_path: rel.to_path_buf(),
                    kind: ChangeKind::Remove,
                    source: None,
                    sha256: None,
                });
            }
        }

        // Rule 2: additions / modifications.
        //
        // Never manage content under persistent paths. When the overlay
        // provides the same path, the overlay change handles it instead of the
        // upstream content being staged here.
        for (path, desired_file) in &desired_files {
            let rel = Path::new(path);
            if self.policy.is_persistent(rel) {
                continue;
            }
            if overlay_files.contains_key(path) {
                continue;
            }
            let source = desired.root.join(&desired_file.rel_path);
            match from.managed_files.get(path) {
                Some(managed) if managed.sha256 != desired_file.sha256 => {
                    modifications.push(FileChange {
                        rel_path: desired_file.rel_path.clone(),
                        kind: ChangeKind::Replace,
                        source: Some(source),
                        sha256: Some(desired_file.sha256.clone()),
                    });
                }
                Some(managed) if live_file_matches(server_root, rel, managed)? => {}
                // A managed file missing from disk or altered outside packctl
                // must be restored from the staged authoritative content.
                Some(_) => {
                    modifications.push(FileChange {
                        rel_path: desired_file.rel_path.clone(),
                        kind: ChangeKind::Replace,
                        source: Some(source),
                        sha256: Some(desired_file.sha256.clone()),
                    });
                }
                None => {
                    additions.push(FileChange {
                        rel_path: desired_file.rel_path.clone(),
                        kind: ChangeKind::Add,
                        source: Some(source),
                        sha256: Some(desired_file.sha256.clone()),
                    });
                }
            }
        }

        // Rule 4: overlay changes.
        //
        // Overlay files are always applied. When an overlay replaces a path
        // that was previously managed, classify whether the upstream version of
        // that path changed relative to what was installed; if it did, surface
        // an informational conflict notice.
        let mut overlay_changes = Vec::new();
        let mut notices = Vec::new();
        for overlay_file in overlay {
            let path = rel_key(&overlay_file.rel_path);
            match from.managed_files.get(&path) {
                Some(managed) => {
                    let upstream_changed = desired_files
                        .get(&path)
                        .is_some_and(|desired| desired.sha256 != managed.sha256);
                    if upstream_changed {
                        overlay_changes.push(OverlayChange {
                            rel_path: overlay_file.rel_path.clone(),
                            status: OverlayChangeStatus::ReplacesChanged,
                        });
                        notices.push(PlanNotice {
                            path: Some(overlay_file.rel_path.clone()),
                            message: conflict_notice(&overlay_file.rel_path),
                        });
                    } else {
                        overlay_changes.push(OverlayChange {
                            rel_path: overlay_file.rel_path.clone(),
                            status: OverlayChangeStatus::ReplacesUnchanged,
                        });
                    }
                }
                None => {
                    overlay_changes.push(OverlayChange {
                        rel_path: overlay_file.rel_path.clone(),
                        status: OverlayChangeStatus::Applied,
                    });
                }
            }
        }

        // Rule 6: deterministic output.
        additions.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        modifications.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        removals.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
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
        })
    }
}

fn live_file_matches(
    server_root: Option<&Path>,
    rel: &Path,
    managed: &crate::core::state::ManagedFile,
) -> Result<bool> {
    let Some(root) = server_root else {
        return Ok(true);
    };
    let path = safe_join(root, rel)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(crate::error::PackError::io(
                format!("stat managed file '{}'", path.display()),
                error,
            ));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != managed.size {
        return Ok(false);
    }
    Ok(sha256_file(&path)? == managed.sha256)
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
    use crate::core::state::ManagedFile;
    use crate::providers::PackVersion;

    const CONFLICT_NOTICE: &str = "Overlay conflict notice\n~ {path}\n  Upstream changed this file.\n  Local overlay replaces it.\n  Overlay version will be used.";

    fn managed(sha: &str, size: u64) -> ManagedFile {
        ManagedFile {
            sha256: sha.to_string(),
            size,
        }
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
                file_id: None,
                released: None,
            },
            root: PathBuf::from("/staging/fixture-pack"),
            files,
        }
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
            .find(|c| c.rel_path == *"config/MiniMOTD/main.conf")
            .expect("overlay change for main.conf");
        assert_eq!(main_conf.status, OverlayChangeStatus::ReplacesChanged);

        let grieflogger = plan
            .overlay_changes
            .iter()
            .find(|c| c.rel_path == *"mods/grieflogger.jar")
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
            .find(|c| c.rel_path == *"mods/custom.jar")
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
            .find(|c| c.rel_path == *"config/example.toml")
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
            .find(|c| c.rel_path == *"config/example.toml")
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
            .find(|c| c.rel_path == *"server.properties")
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
}
