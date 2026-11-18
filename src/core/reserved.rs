//! Paths packctl reserves for its own control files.
//!
//! Pack content is untrusted. A server pack must never be able to write into
//! packctl's state directory, its profile or ignore file, the overlay, or the
//! local-archive drop folder: through the profile it could change the
//! controller's shell commands, through a snapshot it could poison what
//! rollback restores, and through the overlay its content would win over
//! every later update. The same reserved set also keeps the sweep away from
//! those locations.

use std::path::{Path, PathBuf};

use crate::config::profile::{DEFAULT_DROP_DIR, LOCAL_PROFILE, ProviderKind, ServerProfile};
use crate::fs::paths::normalize_relative;

/// packctl's state directory inside the server root.
pub const STATE_DIR: &str = ".packctl";

/// Name of the `.gitignore`-style file that excludes paths from updates.
pub const IGNORE_FILE: &str = ".packctlignore";

/// Default overlay directory name created by `packctl create`.
const DEFAULT_OVERLAY_DIR: &str = "overlay";

/// A set of server-root-relative paths that pack content may never touch.
///
/// A path is reserved when it equals an entry or lies beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedPaths {
    entries: Vec<PathBuf>,
}

impl ReservedPaths {
    /// The reserved set for a server using packctl's default layout: the
    /// state directory, the local profile, the ignore file, `overlay/` and
    /// the `packs/` drop folder.
    pub fn default_layout() -> Self {
        let mut reserved = Self::control_files();
        reserved.push(PathBuf::from(DEFAULT_OVERLAY_DIR));
        reserved.push(PathBuf::from(DEFAULT_DROP_DIR));
        reserved
    }

    /// The reserved set for `profile`: packctl's control files plus the
    /// profile's overlay directory and local archive path, each only when it
    /// lies inside the server root.
    pub fn for_profile(profile: &ServerProfile) -> Self {
        let root = &profile.server.root;
        let mut reserved = Self::control_files();
        if let Some(rel) = relative_inside(root, &profile.overlay.path) {
            reserved.push(rel);
        }
        let archive = match (&profile.pack.archive, profile.pack.provider) {
            (Some(archive), _) => Some(archive.clone()),
            (None, ProviderKind::Local) => Some(root.join(DEFAULT_DROP_DIR)),
            (None, _) => None,
        };
        if let Some(rel) = archive.and_then(|archive| relative_inside(root, &archive)) {
            reserved.push(rel);
        }
        reserved
    }

    fn control_files() -> Self {
        ReservedPaths {
            entries: vec![
                PathBuf::from(STATE_DIR),
                PathBuf::from(LOCAL_PROFILE),
                PathBuf::from(IGNORE_FILE),
            ],
        }
    }

    /// packctl's control files plus `extra` (forward-slash relative paths),
    /// for tests that need a custom overlay or archive location.
    #[cfg(test)]
    pub(crate) fn control_files_and(extra: &[&str]) -> Self {
        let mut reserved = Self::control_files();
        for rel in extra {
            reserved.push(PathBuf::from(rel));
        }
        reserved
    }

    fn push(&mut self, rel: PathBuf) {
        if !self.entries.contains(&rel) {
            self.entries.push(rel);
        }
    }

    /// True when `rel` (relative to the server root) is reserved.
    ///
    /// Paths that cannot be normalized are treated as reserved, so a caller
    /// that forgot to validate a path still refuses it. packctl's control
    /// names (`.packctl`, `.packctl.toml`, `.packctlignore`) match in any
    /// letter case, since on a case-insensitive filesystem `.PACKCTL/` is
    /// the state directory.
    pub fn is_reserved(&self, rel: &Path) -> bool {
        let Ok(rel) = normalize_relative(rel) else {
            return true;
        };
        let control = rel.components().next().is_some_and(|first| {
            let first = first.as_os_str().to_string_lossy();
            [STATE_DIR, LOCAL_PROFILE, IGNORE_FILE]
                .iter()
                .any(|name| first.eq_ignore_ascii_case(name))
        });
        control || self.entries.iter().any(|entry| rel.starts_with(entry))
    }

    /// The reserved entries, relative to the server root.
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }
}

impl Default for ReservedPaths {
    fn default() -> Self {
        Self::default_layout()
    }
}

/// True when `rel` is one of `profile`'s reserved paths (see
/// [`ReservedPaths::for_profile`]).
pub fn is_reserved(rel: &Path, profile: &ServerProfile) -> bool {
    ReservedPaths::for_profile(profile).is_reserved(rel)
}

/// Most reserved paths listed by name in [`skipped_notice`].
const MAX_RESERVED_LISTED: usize = 10;

/// The notice for pack files that target packctl's reserved locations and
/// were skipped. `paths` are forward-slash relative paths.
pub fn skipped_notice(paths: &[String]) -> String {
    let mut message = format!(
        "Skipped {} pack file(s) in locations packctl reserves for itself",
        paths.len()
    );
    for path in paths.iter().take(MAX_RESERVED_LISTED) {
        message.push_str(&format!("\n  {path}"));
    }
    if paths.len() > MAX_RESERVED_LISTED {
        message.push_str(&format!(
            "\n  … and {} more",
            paths.len() - MAX_RESERVED_LISTED
        ));
    }
    message
}

/// The forward-slash form of a relative path, as notices and state use it.
pub fn display_rel(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// `path` relative to `root`, normalized, when it lies strictly inside it.
fn relative_inside(root: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    normalize_relative(rel).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profile::{
        ControllerKind, ControllerSection, OverlaySection, PackSection, SecretsSection,
        ServerSection,
    };

    fn profile(root: &Path, overlay: &Path, archive: Option<PathBuf>) -> ServerProfile {
        ServerProfile {
            name: "test".to_string(),
            server: ServerSection {
                root: root.to_path_buf(),
            },
            pack: PackSection::local(archive),
            overlay: OverlaySection {
                path: overlay.to_path_buf(),
            },
            controller: ControllerSection {
                kind: ControllerKind::Command,
                instance: None,
                command: None,
            },
            secrets: SecretsSection::default(),
        }
    }

    #[test]
    fn control_files_are_always_reserved() {
        let root = PathBuf::from("srv").join("mc");
        let profile = profile(&root, &PathBuf::from("elsewhere"), None);
        for rel in [
            ".packctl/state.json",
            ".packctl/snapshots/x/manifest.json",
            ".packctl.toml",
            ".packctlignore",
        ] {
            assert!(is_reserved(Path::new(rel), &profile), "{rel}");
        }
        // Any letter case, for case-insensitive filesystems.
        for rel in [".PACKCTL/state.json", ".Packctl.TOML", ".PackctlIgnore"] {
            assert!(is_reserved(Path::new(rel), &profile), "{rel}");
        }
        assert!(!is_reserved(Path::new("mods/a.jar"), &profile));
        assert!(!is_reserved(Path::new(".packctl.toml.bak"), &profile));
        assert!(!is_reserved(Path::new("config/.packctl"), &profile));
    }

    #[test]
    fn custom_overlay_and_archive_inside_root_are_reserved() {
        let root = PathBuf::from("srv").join("mc");
        let profile = profile(
            &root,
            &root.join("config").join("my-overlay"),
            Some(root.join("drops").join("pack.zip")),
        );
        assert!(is_reserved(
            Path::new("config/my-overlay/mods/x.jar"),
            &profile
        ));
        assert!(is_reserved(Path::new("drops/pack.zip"), &profile));
        assert!(!is_reserved(Path::new("drops/other.zip"), &profile));
        assert!(!is_reserved(Path::new("config/other.toml"), &profile));
        // The default names are not reserved when the profile uses others.
        assert!(!is_reserved(Path::new("overlay/a.txt"), &profile));
    }

    #[test]
    fn local_provider_without_archive_reserves_the_drop_folder() {
        let root = PathBuf::from("srv").join("mc");
        let profile = profile(&root, &PathBuf::from("outside"), None);
        assert!(is_reserved(Path::new("packs/pack.zip"), &profile));
    }

    #[test]
    fn paths_outside_the_root_reserve_nothing_extra() {
        let root = PathBuf::from("srv").join("mc");
        let profile = profile(
            &root,
            &PathBuf::from("other").join("overlay"),
            Some(PathBuf::from("other").join("packs")),
        );
        assert_eq!(ReservedPaths::for_profile(&profile).entries().len(), 3);
    }

    #[test]
    fn unsafe_paths_count_as_reserved() {
        assert!(ReservedPaths::default_layout().is_reserved(Path::new("../x")));
    }
}
