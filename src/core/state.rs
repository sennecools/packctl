//! Updater state store.
//!
//! Records what the updater knows (installed version, managed files), not
//! arbitrary server data. State lives at `<server_root>/.packctl/state.json`.
//! See design notes "State Store".
//!

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{fs::File, io};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::{PackError, Result};

/// Identity and content fingerprint of a file managed by the updater.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedFile {
    pub sha256: String,
    pub size: u64,
    /// A local variant accepted as equivalent to `sha256`: the live file was
    /// only reformatted (comments, whitespace) when the server was adopted.
    /// While upstream keeps shipping `sha256`, a live file with this hash is
    /// left alone; once upstream changes the file, the new upstream content
    /// is written and the alias is dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_sha256: Option<String>,
}

impl ManagedFile {
    /// A fingerprint from a SHA-256 hex digest and a size in bytes.
    pub fn new(sha256: impl Into<String>, size: u64) -> Self {
        ManagedFile {
            sha256: sha256.into(),
            size,
            local_sha256: None,
        }
    }

    /// The same fingerprint with an accepted local variant (see
    /// [`ManagedFile::local_sha256`]).
    pub fn with_local(mut self, local_sha256: impl Into<String>) -> Self {
        self.local_sha256 = Some(local_sha256.into());
        self
    }
}

/// How a hand-installed server was recorded by `packctl adopt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptionRecord {
    /// When the adoption was applied.
    pub at: DateTime<Utc>,
    /// The pack version name the server was recorded as.
    pub version: String,
    pub provider_version_id: String,
    /// Files adopt copied from the server into the overlay (relative paths).
    pub overlay_added: Vec<String>,
    /// The `.packctlignore` rules adopt wrote in its managed block.
    pub ignore_rules: Vec<String>,
}

/// Everything the updater has persisted about an installed server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledState {
    pub installed_version: Option<String>,
    pub provider_version_id: Option<String>,
    /// The provider the recorded version came from (`curseforge`,
    /// `modrinth`, `local`). Absent in state written by older packctl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The provider project (id, or slug when there is no id) the recorded
    /// version belongs to. Absent for local archives and older state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The Minecraft version the recorded pack version targets, when known.
    /// Updates to "latest" stay on this Minecraft version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minecraft: Option<String>,
    /// Relative paths (forward slashes) keyed to their file fingerprints.
    pub managed_files: HashMap<String, ManagedFile>,
    /// Upstream fingerprints of pack paths the overlay owns.
    ///
    /// The overlay replaces these paths, so they are not upstream-managed, but
    /// remembering the upstream content lets every later update tell whether
    /// upstream changed a file the overlay hides.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub overlay_shadowed: HashMap<String, ManagedFile>,
    /// Fingerprints of the overlay files as they were last written to the
    /// server, so a live file changed after that can be told apart from an
    /// overlay file the admin edited.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub overlay_applied: HashMap<String, ManagedFile>,
    pub last_successful_update: Option<DateTime<Utc>>,
    /// Set when `packctl adopt` recorded this server. Kept by later updates
    /// so `status` can say how packctl came to manage it. Absent in state
    /// written before adoption existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted: Option<AdoptionRecord>,
}

impl InstalledState {
    /// True when no version is recorded, no files are tracked, and no update
    /// has ever completed.
    pub fn is_empty(&self) -> bool {
        self.installed_version.is_none()
            && self.provider_version_id.is_none()
            && self.managed_files.is_empty()
            && self.overlay_shadowed.is_empty()
            && self.overlay_applied.is_empty()
            && self.last_successful_update.is_none()
    }
}

/// JSON state store bound to a server root.
pub struct StateStore {
    /// Absolute path of the state file (`<server_root>/.packctl/state.json`).
    pub path: PathBuf,
}

/// An exclusive per-server lock held while an update or rollback mutates files.
pub struct ServerLock {
    file: File,
}

impl Drop for ServerLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl StateStore {
    /// Returns the store for `server_root`.
    ///
    /// Does not create the file or any directory.
    pub fn at(server_root: &Path) -> Result<Self> {
        Ok(StateStore {
            path: server_root.join(".packctl").join("state.json"),
        })
    }

    /// Loads the persisted state.
    ///
    /// A missing file yields an empty default state. Corrupt JSON is surfaced
    /// as [`PackError::State`] naming the offending path rather than being
    /// silently ignored.
    pub fn load(&self) -> Result<InstalledState> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(InstalledState::default());
            }
            Err(e) => {
                return Err(PackError::io(
                    format!("read state file '{}'", self.path.display()),
                    e,
                ));
            }
        };
        serde_json::from_slice(&bytes).map_err(|e| {
            PackError::State(format!(
                "failed to parse state file '{}': {e}",
                self.path.display()
            ))
        })
    }

    /// Acquires the exclusive mutation lock for this server without waiting.
    pub fn lock(&self) -> Result<ServerLock> {
        let parent = self.path.parent().ok_or_else(|| {
            PackError::State(format!(
                "state path '{}' has no parent",
                self.path.display()
            ))
        })?;
        std::fs::create_dir_all(parent).map_err(|error| {
            PackError::io(
                format!("create state directory '{}'", parent.display()),
                error,
            )
        })?;
        let lock_path = parent.join("update.lock");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                PackError::io(format!("open update lock '{}'", lock_path.display()), error)
            })?;
        file.try_lock_exclusive().map_err(|error| {
            let message = if error.kind() == io::ErrorKind::WouldBlock {
                "another packctl update or rollback is already running".to_string()
            } else {
                format!("lock update operation: {error}")
            };
            PackError::State(message)
        })?;
        Ok(ServerLock { file })
    }

    /// Persists `state` atomically.
    ///
    /// Parent directories are created first, then the JSON is written to a
    /// sibling temporary file and renamed over the target. Failures are
    /// wrapped with context naming the state path.
    pub fn save(&self, state: &InstalledState) -> Result<()> {
        let json = serde_json::to_vec_pretty(state).map_err(|e| {
            PackError::State(format!(
                "failed to serialize state for '{}': {e}",
                self.path.display()
            ))
        })?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| {
                PackError::io(format!("create state directory '{}'", parent.display()), e)
            })?;
        }
        write_atomically(&self.path, &json, "state file")
    }
}

/// Writes `bytes` to `path` atomically: into a fresh temporary sibling that
/// is synced and then renamed over `path`, so a reader never sees a partial
/// file. `what` names the file in errors.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8], what: &str) -> Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    for counter in 0u32.. {
        let tmp = path.with_file_name(format!(
            ".{}.{}.{}.tmp",
            file_name,
            std::process::id(),
            counter
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut file) => {
                use std::io::Write;
                if let Err(error) = file
                    .write_all(bytes)
                    .and_then(|_| file.sync_all())
                    .and_then(|_| std::fs::rename(&tmp, path))
                {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(PackError::io(
                        format!("replace {what} '{}'", path.display()),
                        error,
                    ));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PackError::io(
                    format!("create temporary {what} '{}'", tmp.display()),
                    error,
                ));
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn timestamp(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn full_state() -> InstalledState {
        let mut files = HashMap::new();
        files.insert("mods/a.jar".to_string(), ManagedFile::new("abc123", 12));
        files.insert("config/b.toml".to_string(), ManagedFile::new("def456", 34));
        InstalledState {
            installed_version: Some("4.12".to_string()),
            provider_version_id: Some("provider-9".to_string()),
            managed_files: files,
            overlay_shadowed: [(
                "config/shadowed.toml".to_string(),
                ManagedFile::new("upstream", 8),
            )]
            .into_iter()
            .collect(),
            overlay_applied: [(
                "config/shadowed.toml".to_string(),
                ManagedFile::new("overlay", 7),
            )]
            .into_iter()
            .collect(),
            provider: Some("curseforge".to_string()),
            project: Some("925200".to_string()),
            minecraft: Some("1.21.1".to_string()),
            last_successful_update: Some(timestamp("2026-08-14T12:00:00Z")),
            adopted: Some(AdoptionRecord {
                at: timestamp("2026-08-01T09:30:00Z"),
                version: "4.11".to_string(),
                provider_version_id: "provider-8".to_string(),
                overlay_added: vec!["mods/extra.jar".to_string()],
                ignore_rules: vec!["config/luckperms".to_string()],
            }),
        }
    }

    #[test]
    fn state_json_without_adopted_or_local_sha256_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(
            &store.path,
            br#"{
  "installed_version": "4.11",
  "provider_version_id": "11",
  "managed_files": {
    "config/a.toml": { "sha256": "abc", "size": 3 }
  },
  "last_successful_update": null
}"#,
        )
        .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.adopted, None);
        assert_eq!(loaded.managed_files["config/a.toml"].local_sha256, None);

        // Absent values are not written either.
        let json = serde_json::to_string(&loaded).unwrap();
        assert!(!json.contains("adopted"), "json: {json}");
        assert!(!json.contains("local_sha256"), "json: {json}");

        // A recorded alias round-trips.
        let mut aliased = loaded.clone();
        aliased.managed_files.insert(
            "config/a.toml".to_string(),
            ManagedFile::new("abc", 3).with_local("def"),
        );
        store.save(&aliased).unwrap();
        assert_eq!(
            store.load().unwrap().managed_files["config/a.toml"]
                .local_sha256
                .as_deref(),
            Some("def")
        );
    }

    #[test]
    fn state_json_without_overlay_shadowed_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        // The layout written before overlay tracking existed.
        std::fs::write(
            &store.path,
            br#"{
  "installed_version": "4.11",
  "provider_version_id": "11",
  "managed_files": {
    "mods/a.jar": { "sha256": "abc", "size": 3 }
  },
  "last_successful_update": "2026-08-14T12:00:00Z"
}"#,
        )
        .unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded.installed_version.as_deref(), Some("4.11"));
        assert_eq!(loaded.managed_files.len(), 1);
        assert!(loaded.overlay_shadowed.is_empty());
        assert!(loaded.overlay_applied.is_empty());
    }

    #[test]
    fn state_json_without_provider_fields_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        // The layout written before the provider and project were recorded.
        std::fs::write(
            &store.path,
            br#"{
  "installed_version": "4.11",
  "provider_version_id": "11",
  "managed_files": {},
  "last_successful_update": null
}"#,
        )
        .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.provider, None);
        assert_eq!(loaded.project, None);
        assert_eq!(loaded.provider_version_id.as_deref(), Some("11"));

        // Absent values are not written, so older packctl can still read it.
        let json = serde_json::to_string(&loaded).unwrap();
        assert!(!json.contains("\"provider\""), "json: {json}");
        assert!(!json.contains("\"project\""), "json: {json}");
    }

    #[test]
    fn empty_overlay_maps_are_not_serialized() {
        let json = serde_json::to_string(&InstalledState::default()).unwrap();
        assert!(!json.contains("overlay_shadowed"), "json: {json}");
        assert!(!json.contains("overlay_applied"), "json: {json}");
    }

    #[test]
    fn overlay_maps_make_state_non_empty() {
        let mut state = InstalledState::default();
        state
            .overlay_shadowed
            .insert("config/a.toml".to_string(), ManagedFile::new("x", 1));
        assert!(!state.is_empty());
    }

    #[test]
    fn save_load_round_trip_preserves_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();
        let state = full_state();

        store.save(&state).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded, state);
        assert_eq!(loaded.installed_version.as_deref(), Some("4.12"));
        assert_eq!(loaded.managed_files["mods/a.jar"].sha256, "abc123");
        assert_eq!(
            loaded.last_successful_update,
            Some(timestamp("2026-08-14T12:00:00Z"))
        );
    }

    #[test]
    fn save_load_round_trip_default_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();

        store.save(&InstalledState::default()).unwrap();
        let loaded = store.load().unwrap();

        assert!(loaded.is_empty());
        assert_eq!(loaded, InstalledState::default());
        assert!(loaded.managed_files.is_empty());
        assert!(loaded.last_successful_update.is_none());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();

        assert!(!store.path.exists());
        let state = store.load().unwrap();

        assert!(state.is_empty());
    }

    #[test]
    fn load_corrupt_json_returns_state_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, b"{ this is not json").unwrap();

        match store.load() {
            Err(PackError::State(msg)) => {
                assert!(
                    msg.contains("state.json"),
                    "message should name the state path: {msg}"
                );
            }
            other => panic!("expected PackError::State, got {other:?}"),
        }
    }

    #[test]
    fn save_creates_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("a/b");
        let store = StateStore::at(&root).unwrap();

        store.save(&InstalledState::default()).unwrap();

        assert!(store.path.is_file());
        assert!(root.join(".packctl").is_dir());
    }

    #[test]
    fn save_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();

        store.save(&full_state()).unwrap();
        store.save(&InstalledState::default()).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(".packctl"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn save_avoids_existing_temporary_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        let collision = store
            .path
            .with_file_name(format!(".state.json.{}.0.tmp", std::process::id()));
        std::fs::write(&collision, b"other writer").unwrap();
        store.save(&full_state()).unwrap();
        assert_eq!(std::fs::read(&collision).unwrap(), b"other writer");
        assert_eq!(store.load().unwrap(), full_state());
    }

    #[test]
    fn lock_rejects_concurrent_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path()).unwrap();
        let _first = store.lock().unwrap();
        assert!(matches!(store.lock(), Err(PackError::State(_))));
    }
}
