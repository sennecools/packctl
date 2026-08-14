//! Rollback snapshots.
//!
//! A snapshot is a rollback point created before mutating managed server
//! files. Snapshots preserve updater-managed state only; they deliberately do
//! not duplicate large persistent runtime data such as `world/` (see design notes
//! "Snapshot" and "Rollback").
//!
//! Layout:
//! ```text
//! <server_root>/.packctl/snapshots/<timestamp>/
//! ├── manifest.json
//! └── files/<relative path of each snapshotted file>
//! ```
//!
// The public API in this module is consumed by the planner/executor modules,
// which are not implemented yet. Allow dead_code so the ready API surface does
// not emit warnings while the rest of the core is being built out.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::state::ManagedFile;
use crate::error::{PackError, Result};
use crate::fs::copy::{copy_file, remove_file};
use crate::fs::hashing::sha256_file;
use crate::fs::paths::{safe_join, strip_server_root};

/// Directory name timestamp format, e.g. `2026-08-14T22-41-10Z`.
const SNAPSHOT_STAMP_FORMAT: &str = "%Y-%m-%dT%H-%M-%SZ";

/// What a snapshot captured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub created: DateTime<Utc>,
    /// Relative path (forward slashes) -> info for each snapshotted file.
    pub files: HashMap<String, ManagedFile>,
    /// Every relative path the update planned to touch
    /// (add/modify/remove/overlay).
    pub tracked_paths: Vec<String>,
}

/// A rollback point on disk.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Absolute path of the snapshot directory.
    pub dir: PathBuf,
    pub created: DateTime<Utc>,
    /// In-memory copy of `dir/manifest.json`.
    pub manifest: SnapshotManifest,
}

impl Snapshot {
    /// Path of the snapshot's `manifest.json`.
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    /// Directory holding the copied managed files.
    pub fn files_root(&self) -> PathBuf {
        self.dir.join("files")
    }
}

/// Creates a snapshot of `files` before mutating them.
///
/// `files` are absolute paths to existing managed/overlay files that will be
/// touched; each is copied into the snapshot's `files/` subdir preserving its
/// relative path under `server_root`, and hashed for the manifest. Files that
/// do not exist (NotFound) or are not regular files are skipped. `tracked_paths`
/// records every relative path the update planned to touch.
pub fn create_snapshot(
    server_root: &Path,
    files: &[&Path],
    tracked_paths: &[&str],
) -> Result<Snapshot> {
    let created = Utc::now();
    let snapshots_root = server_root.join(".packctl").join("snapshots");
    let dir = unique_snapshot_dir(&snapshots_root, created);
    std::fs::create_dir_all(dir.join("files"))
        .map_err(|e| PackError::io(format!("create snapshot directory '{}'", dir.display()), e))?;

    let mut manifest_files = HashMap::new();
    for file in files {
        let rel = strip_server_root(server_root, file)?;
        let metadata = match std::fs::metadata(file) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(PackError::io(
                    format!("stat '{}' while creating snapshot", file.display()),
                    e,
                ));
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let dest = dir.join("files").join(&rel);
        copy_file(file, &dest)?;
        let sha256 = sha256_file(file)?;
        manifest_files.insert(
            rel_str,
            ManagedFile {
                sha256,
                size: metadata.len(),
            },
        );
    }

    let manifest = SnapshotManifest {
        created,
        files: manifest_files,
        tracked_paths: tracked_paths.iter().map(|s| s.to_string()).collect(),
    };
    write_manifest(&dir, &manifest)?;

    Ok(Snapshot {
        dir,
        created,
        manifest,
    })
}

/// Restores the server to the state captured by `snapshot`.
///
/// The manifest is always loaded from `snapshot.dir/manifest.json` so a
/// snapshot can be restored even when only its directory is known.
///
/// 1. Every `tracked_path` not present in the manifest's `files` is removed
///    from `server_root` if it exists (these were files a failed update added).
/// 2. Every snapshotted file is copied back over `<server_root>/<rel>`,
///    recreating parent directories. A failed restore copy surfaces a clear
///    contextual error rather than leaving the server half-restored silently.
///
/// Persistent runtime data (`world/`, `logs/`, ...) is never touched; callers
/// only pass managed paths as `tracked_paths` and `files`.
pub fn restore_snapshot(server_root: &Path, snapshot: &Snapshot) -> Result<()> {
    let manifest = load_manifest(&snapshot.dir)?;
    let files_root = snapshot.dir.join("files");

    for tracked in &manifest.tracked_paths {
        if !manifest.files.contains_key(tracked) {
            let target = safe_join(server_root, Path::new(tracked))?;
            remove_file(&target)?;
        }
    }

    for rel in manifest.files.keys() {
        let source = files_root.join(rel_path_from_string(rel));
        if !source.exists() {
            return Err(PackError::State(format!(
                "snapshot '{}' is missing restore file '{}'",
                snapshot.dir.display(),
                rel
            )));
        }
        let target = safe_join(server_root, Path::new(rel))?;
        copy_file(&source, &target)?;
    }
    Ok(())
}

/// Lists snapshots newest-first.
///
/// Missing snapshots directory yields an empty list. A snapshot whose
/// `manifest.json` is unreadable or corrupt is surfaced as an error so that
/// corruption is visible rather than silently skipped.
pub fn list_snapshots(server_root: &Path) -> Result<Vec<Snapshot>> {
    let snapshots_root = server_root.join(".packctl").join("snapshots");
    let entries = match std::fs::read_dir(&snapshots_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(PackError::io(
                format!("list snapshots in '{}'", snapshots_root.display()),
                e,
            ));
        }
    };

    let mut snapshots = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            PackError::io(
                format!("read snapshot directory '{}'", snapshots_root.display()),
                e,
            )
        })?;
        let file_type = entry.file_type().map_err(|e| {
            PackError::io(
                format!("stat snapshot entry '{}'", entry.path().display()),
                e,
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let dir = entry.path();
        let manifest = load_manifest(&dir)?;
        snapshots.push(Snapshot {
            dir,
            created: manifest.created,
            manifest,
        });
    }

    snapshots.sort_by(|a, b| {
        b.created
            .cmp(&a.created)
            .then_with(|| b.dir.file_name().cmp(&a.dir.file_name()))
    });
    Ok(snapshots)
}

/// Reads and parses `dir/manifest.json`.
fn load_manifest(dir: &Path) -> Result<SnapshotManifest> {
    let path = dir.join("manifest.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(PackError::NotFound(format!(
                "snapshot manifest not found: '{}'",
                path.display()
            )));
        }
        Err(e) => {
            return Err(PackError::io(
                format!("read snapshot manifest '{}'", path.display()),
                e,
            ));
        }
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        PackError::State(format!(
            "failed to parse snapshot manifest '{}': {e}",
            path.display()
        ))
    })
}

/// Writes `manifest` as `dir/manifest.json`.
fn write_manifest(dir: &Path, manifest: &SnapshotManifest) -> Result<()> {
    let path = dir.join("manifest.json");
    let json = serde_json::to_vec_pretty(manifest).map_err(|e| {
        PackError::State(format!(
            "failed to serialize snapshot manifest '{}': {e}",
            path.display()
        ))
    })?;
    std::fs::write(&path, json)
        .map_err(|e| PackError::io(format!("write snapshot manifest '{}'", path.display()), e))
}

/// Chooses a snapshot directory name based on `created`.
///
/// The timestamp format truncates to seconds; on the rare collision the name
/// gets a short counter suffix.
fn unique_snapshot_dir(snapshots_root: &Path, created: DateTime<Utc>) -> PathBuf {
    let stamp = created.format(SNAPSHOT_STAMP_FORMAT).to_string();
    let base = snapshots_root.join(&stamp);
    if !base.exists() {
        return base;
    }
    let mut counter = 1u32;
    loop {
        let candidate = snapshots_root.join(format!("{stamp}-{counter}"));
        if !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}

/// Rebuilds a relative path from a forward-slash string.
fn rel_path_from_string(rel: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for component in rel.split(['/', '\\']) {
        path.push(component);
    }
    path
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;
    use crate::fs::hashing::sha256_bytes;

    /// Creates a server root with a couple of managed files.
    fn make_server(tmp: &tempfile::TempDir) -> PathBuf {
        let root = tmp.path().join("server");
        std::fs::create_dir_all(root.join("mods")).unwrap();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("mods/a.jar"), b"AAAA").unwrap();
        std::fs::write(root.join("config/b.toml"), b"BBBB").unwrap();
        root
    }

    fn paths_as_ref(paths: &[PathBuf]) -> Vec<&Path> {
        paths.iter().map(PathBuf::as_path).collect()
    }

    #[test]
    fn create_snapshot_stores_copies_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_server(&tmp);
        let paths = [root.join("mods/a.jar"), root.join("config/b.toml")];
        let tracked = ["mods/a.jar", "config/b.toml"];

        let snap = create_snapshot(&root, &paths_as_ref(&paths), &tracked).unwrap();

        assert!(snap.manifest_path().is_file());
        assert!(snap.files_root().join("mods/a.jar").is_file());
        assert!(snap.files_root().join("config/b.toml").is_file());

        assert_eq!(snap.manifest.files.len(), 2);
        assert_eq!(snap.manifest.files["mods/a.jar"].size, 4);
        assert_eq!(
            snap.manifest.files["mods/a.jar"].sha256,
            sha256_bytes(b"AAAA")
        );
        assert_eq!(snap.manifest.files["config/b.toml"].size, 4);
        assert_eq!(
            snap.manifest.files["config/b.toml"].sha256,
            sha256_bytes(b"BBBB")
        );
        assert_eq!(snap.manifest.tracked_paths, tracked);

        let reloaded = load_manifest(&snap.dir).unwrap();
        assert_eq!(reloaded.created, snap.manifest.created);
        assert_eq!(reloaded.files, snap.manifest.files);
        assert_eq!(reloaded.tracked_paths, tracked);
    }

    #[test]
    fn create_snapshot_skips_missing_and_non_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_server(&tmp);
        std::fs::create_dir_all(root.join("mods/sub")).unwrap();
        let paths = [
            root.join("mods/a.jar"),
            root.join("mods/does-not-exist.jar"),
            root.join("mods/sub"),
        ];
        let tracked = ["mods/a.jar", "mods/does-not-exist.jar", "mods/sub"];

        let snap = create_snapshot(&root, &paths_as_ref(&paths), &tracked).unwrap();

        assert_eq!(snap.manifest.files.len(), 1);
        assert!(!snap.manifest.files.contains_key("mods/does-not-exist.jar"));
        assert!(!snap.manifest.files.contains_key("mods/sub"));
        assert_eq!(snap.manifest.tracked_paths.len(), 3);
    }

    #[test]
    fn restore_restores_originals_and_removes_new_tracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_server(&tmp);
        let paths = [
            root.join("mods/a.jar"),
            root.join("config/b.toml"),
            root.join("mods/new.jar"),
        ];
        let tracked = ["mods/a.jar", "config/b.toml", "mods/new.jar"];

        let snap = create_snapshot(&root, &paths_as_ref(&paths), &tracked).unwrap();
        assert_eq!(snap.manifest.files.len(), 2, "new.jar is skipped");

        std::fs::write(root.join("mods/a.jar"), b"CHANGED").unwrap();
        std::fs::write(root.join("config/b.toml"), b"CHANGED").unwrap();
        std::fs::write(root.join("mods/new.jar"), b"NEW").unwrap();

        restore_snapshot(&root, &snap).unwrap();

        assert_eq!(std::fs::read(root.join("mods/a.jar")).unwrap(), b"AAAA");
        assert_eq!(std::fs::read(root.join("config/b.toml")).unwrap(), b"BBBB");
        assert!(
            !root.join("mods/new.jar").exists(),
            "newly added tracked file should be removed"
        );
    }

    #[test]
    fn restore_does_not_touch_untracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_server(&tmp);
        std::fs::create_dir_all(root.join("world/region")).unwrap();
        std::fs::write(root.join("world/region/r.mca"), b"WORLD").unwrap();
        std::fs::write(root.join("unknown.txt"), b"UNTRACKED").unwrap();

        let paths = [root.join("mods/a.jar")];
        let snap = create_snapshot(&root, &paths_as_ref(&paths), &["mods/a.jar"]).unwrap();

        std::fs::write(root.join("mods/a.jar"), b"MUTATED").unwrap();
        std::fs::write(root.join("world/region/r.mca"), b"TOUCHED?").unwrap();
        std::fs::write(root.join("unknown.txt"), b"TOUCHED?").unwrap();

        restore_snapshot(&root, &snap).unwrap();

        assert_eq!(std::fs::read(root.join("mods/a.jar")).unwrap(), b"AAAA");
        assert_eq!(
            std::fs::read(root.join("world/region/r.mca")).unwrap(),
            b"TOUCHED?",
            "persistent world data must not be touched"
        );
        assert_eq!(
            std::fs::read(root.join("unknown.txt")).unwrap(),
            b"TOUCHED?",
            "untracked files must not be touched"
        );
    }

    #[test]
    fn list_snapshots_returns_newest_first_and_empty_when_none() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_server(&tmp);

        assert!(list_snapshots(&root).unwrap().is_empty());

        let paths = [root.join("mods/a.jar")];
        let first = create_snapshot(&root, &paths_as_ref(&paths), &["mods/a.jar"]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let second = create_snapshot(&root, &paths_as_ref(&paths), &["mods/a.jar"]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let third = create_snapshot(&root, &paths_as_ref(&paths), &["mods/a.jar"]).unwrap();

        let list = list_snapshots(&root).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].dir, third.dir);
        assert_eq!(list[1].dir, second.dir);
        assert_eq!(list[2].dir, first.dir);
    }

    #[test]
    fn restore_uses_manifest_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_server(&tmp);
        let paths = [root.join("mods/a.jar"), root.join("config/b.toml")];
        let snap = create_snapshot(
            &root,
            &paths_as_ref(&paths),
            &["mods/a.jar", "config/b.toml"],
        )
        .unwrap();

        std::fs::write(root.join("mods/a.jar"), b"CHANGED").unwrap();

        let disk_manifest = load_manifest(&snap.dir).unwrap();
        let misleading = Snapshot {
            dir: snap.dir.clone(),
            created: disk_manifest.created,
            manifest: SnapshotManifest {
                created: disk_manifest.created,
                files: HashMap::new(),
                tracked_paths: Vec::new(),
            },
        };

        restore_snapshot(&root, &misleading).unwrap();

        assert_eq!(
            std::fs::read(root.join("mods/a.jar")).unwrap(),
            b"AAAA",
            "restore must trust the manifest on disk, not the in-memory copy"
        );
    }
}
