//! Central filesystem classification.
//!
//! Every destructive filesystem operation should pass through this policy
//! abstraction instead of duplicating ad-hoc `path != "world"` checks across
//! modules (see design notes "Filesystem Policy").
//!
//! The policy distinguishes persistent runtime data (never replaced merely
//! because it is absent from a new modpack version) from updater-managed files.
//! `server.properties` is persistent by default, but is intentionally managed
//! when explicitly present in the overlay; the planner/overlay layer handles
//! that interaction.
//!
// The public API in this module is consumed by the planner/executor modules,
// which are not implemented yet. Allow dead_code so the ready API surface does
// not emit warnings while the rest of the core is being built out.
#![allow(dead_code)]

use std::path::Path;

/// How a path is owned relative to the updater.
///
/// `Overlay` and `Unknown` are not decided here: the planner derives them from
/// its inputs (overlay set, managed state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileClass {
    /// Content whose lifecycle is controlled by the upstream modpack state.
    Managed,
    /// Runtime data that must not be replaced by an update.
    Persistent,
    /// Provided by the local mirrored overlay.
    Overlay,
    /// Not classified by any known policy.
    Unknown,
}

/// Which relative paths count as persistent runtime data.
#[derive(Debug, Clone, Default)]
pub struct FilePolicy {
    /// Top-level directory names treated as persistent (e.g. `world`, `logs`).
    pub persistent_dirs: Vec<String>,
    /// Top-level file names treated as persistent (e.g. `ops.json`).
    pub persistent_files: Vec<String>,
}

impl FilePolicy {
    /// The default persistence policy documented in design notes.
    ///
    /// These are runtime data that must not be replaced merely because they
    /// are absent from a new modpack version. `server.properties` is included
    /// by default but may be managed explicitly via the overlay.
    pub fn default_policy() -> Self {
        FilePolicy {
            persistent_dirs: vec![
                "world".to_string(),
                "logs".to_string(),
                "backups".to_string(),
                "crash-reports".to_string(),
            ],
            persistent_files: vec![
                "server.properties".to_string(),
                "ops.json".to_string(),
                "whitelist.json".to_string(),
                "banned-players.json".to_string(),
                "banned-ips.json".to_string(),
                "usercache.json".to_string(),
            ],
        }
    }

    /// Returns true when `rel` is persistent.
    ///
    /// A relative path is persistent when its first path component equals one
    /// of `persistent_dirs` (matching `world`, `world/region/r.mca`, and so
    /// on) or when the whole path equals one of `persistent_files` at the top
    /// level. Prefix-like matching (e.g. `world_` matching `world`) is never
    /// invented; only the documented names are honored.
    pub fn is_persistent(&self, rel: &Path) -> bool {
        if rel.as_os_str().is_empty() {
            return false;
        }
        let s = rel.to_string_lossy();
        let first = s.split(['/', '\\']).next().unwrap_or_default();
        if self.persistent_dirs.iter().any(|d| d.as_str() == first) {
            return true;
        }
        self.persistent_files
            .iter()
            .any(|f| f.as_str() == s.as_ref())
    }

    /// Classifies `rel` as [`FileClass::Persistent`] or [`FileClass::Managed`].
    ///
    /// `Overlay` and `Unknown` are determined by the planner, not here.
    pub fn classify(&self, rel: &Path) -> FileClass {
        if self.is_persistent(rel) {
            FileClass::Persistent
        } else {
            FileClass::Managed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn default_policy_marks_runtime_data_persistent() {
        let policy = FilePolicy::default_policy();
        for p in [
            "world",
            "world/",
            "world/region/r.0.0.mca",
            "logs/latest.log",
            "backups/x",
            "crash-reports/crash.txt",
            "server.properties",
            "ops.json",
            "whitelist.json",
            "banned-players.json",
            "banned-ips.json",
            "usercache.json",
        ] {
            assert!(
                policy.is_persistent(Path::new(p)),
                "{p} should be persistent"
            );
        }
    }

    #[test]
    fn default_policy_marks_managed_paths_not_persistent() {
        let policy = FilePolicy::default_policy();
        for p in [
            "mods/x.jar",
            "config/y.toml",
            "world_nether/region/r.mca",
            "worlds/x",
            "config/server.properties",
            "config/ops.json",
            "run.sh",
            "version.json",
            "",
        ] {
            assert!(
                !policy.is_persistent(Path::new(p)),
                "{p} should not be persistent"
            );
        }
    }

    #[test]
    fn classify_returns_the_right_enum() {
        let policy = FilePolicy::default_policy();
        assert_eq!(
            policy.classify(Path::new("world/data.dat")),
            FileClass::Persistent
        );
        assert_eq!(
            policy.classify(Path::new("ops.json")),
            FileClass::Persistent
        );
        assert_eq!(
            policy.classify(Path::new("mods/foo.jar")),
            FileClass::Managed
        );
        assert_eq!(policy.classify(Path::new("")), FileClass::Managed);
    }

    #[test]
    fn custom_policy_respects_its_lists() {
        let policy = FilePolicy {
            persistent_dirs: vec!["custom".to_string()],
            persistent_files: vec!["keep.txt".to_string()],
        };
        assert!(policy.is_persistent(Path::new("custom/deep/file")));
        assert!(policy.is_persistent(Path::new("custom")));
        assert!(policy.is_persistent(Path::new("keep.txt")));
        assert!(!policy.is_persistent(Path::new("other/keep.txt")));
        assert!(!policy.is_persistent(Path::new("customs/x")));
        assert!(!policy.is_persistent(Path::new("nope.txt")));
    }
}
