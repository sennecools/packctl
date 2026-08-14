//! Mirrored local overlay engine.
//!
//! The overlay is a local directory whose structure mirrors the server root;
//! files in it are copied over the upstream installation after an update and
//! always win over upstream content (see design notes "Overlay").
//!
//! The engine is intentionally simple and file-type-agnostic: it walks the
//! overlay, validates and resolves relative destination paths, and copies
//! changed files. It knows nothing about upstream files or persistent-data
//! policy — the planner decides overlay precedence and conflict reporting.
//!

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{PackError, Result};
use crate::fs::copy::copy_if_changed;
use crate::fs::hashing::sha256_file;
use crate::fs::paths::{normalize_relative, safe_join, strip_server_root};

/// A single file inside the overlay ready to be applied.
#[derive(Debug, Clone)]
pub struct OverlayFile {
    /// Destination path relative to the server root (forward slashes).
    pub rel_path: PathBuf,
    /// Absolute path of the file inside the overlay.
    pub source: PathBuf,
    pub sha256: String,
    pub size: u64,
}

/// Walks, validates, and applies a mirrored overlay directory.
pub struct OverlayEngine {
    /// Root of the overlay directory (absolute).
    pub root: PathBuf,
}

impl OverlayEngine {
    pub fn new(root: PathBuf) -> Self {
        OverlayEngine { root }
    }

    /// Walks the overlay and returns one entry per file, sorted by `rel_path`.
    ///
    /// Symlinks are not followed. Returns an empty vector when the overlay
    /// root does not exist. An unsafe overlay path (absolute, `..`, empty
    /// components, NUL bytes) is a hard error and is never silently skipped.
    pub fn scan(&self) -> Result<Vec<OverlayFile>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        for entry in WalkDir::new(&self.root).follow_links(false) {
            let entry = entry.map_err(|err| {
                let message = err.to_string();
                let io_error = err
                    .into_io_error()
                    .unwrap_or_else(|| std::io::Error::other(message));
                PackError::io(format!("walk overlay '{}'", self.root.display()), io_error)
            })?;
            if !entry.file_type().is_file() {
                continue;
            }

            let source = entry.path();
            let rel = strip_server_root(&self.root, source)?;
            let rel_path = validate_overlay_path(&rel)?;

            let sha256 = sha256_file(source)?;
            let metadata = std::fs::metadata(source).map_err(|e| {
                PackError::io(
                    format!(
                        "stat '{}' while scanning overlay '{}'",
                        source.display(),
                        self.root.display()
                    ),
                    e,
                )
            })?;

            files.push(OverlayFile {
                rel_path,
                source: source.to_path_buf(),
                sha256,
                size: metadata.len(),
            });
        }

        files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        Ok(files)
    }

    /// Copies every given file into `server_root`, skipping files already
    /// present with identical contents, and returns how many were copied.
    ///
    /// Destination paths are resolved against `server_root` via `safe_join`;
    /// unsafe relative paths are rejected with an error naming the
    /// destination. Parent directories are created as needed.
    pub fn apply(&self, files: &[OverlayFile], server_root: &Path) -> Result<usize> {
        let mut copied = 0;
        for file in files {
            let dest = safe_join(server_root, &file.rel_path)?;
            if copy_if_changed(&file.source, &dest).map_err(|e| apply_context(&dest, e))? {
                copied += 1;
            }
        }
        Ok(copied)
    }
}

/// Validates a relative overlay path and returns its normalized form.
///
/// Overlay paths are untrusted. Backslashes are converted to forward slashes,
/// and absolute paths, `..`, `.`, empty components, and NUL bytes are
/// rejected.
pub(crate) fn validate_overlay_path(rel: &Path) -> Result<PathBuf> {
    normalize_relative(rel)
}

/// Adds destination context to an error raised while applying an overlay file.
///
/// Copy/hash helpers name the paths they operated on, but not necessarily the
/// resolved destination; this keeps the failure actionable (see design notes
/// "Error Messages").
fn apply_context(dest: &Path, error: PackError) -> PackError {
    match error {
        PackError::Io { operation, source } => PackError::Io {
            operation: format!("{operation} (applying overlay to '{}')", dest.display()),
            source,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::hashing::sha256_bytes;

    /// Writes `data` to `overlay/rel`, creating parent directories.
    fn write_file(overlay: &Path, rel: &str, data: &[u8]) {
        let path = overlay.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn scan_returns_files_sorted_by_rel_path_with_hash_and_size() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay = tmp.path().join("overlay");
        write_file(&overlay, "mods/z.jar", b"zzz");
        write_file(&overlay, "mods/a.jar", b"aaa");
        write_file(&overlay, "config/b/deep.toml", b"deep");
        write_file(&overlay, "top.txt", b"top");

        let engine = OverlayEngine::new(overlay.clone());
        let files = engine.scan().unwrap();

        let rels: Vec<PathBuf> = files.iter().map(|f| f.rel_path.clone()).collect();
        assert_eq!(
            rels,
            vec![
                PathBuf::from("config/b/deep.toml"),
                PathBuf::from("mods/a.jar"),
                PathBuf::from("mods/z.jar"),
                PathBuf::from("top.txt"),
            ]
        );

        for file in &files {
            assert_eq!(file.source, overlay.join(&file.rel_path));
            let contents = std::fs::read(&file.source).unwrap();
            assert_eq!(file.size, contents.len() as u64);
            assert_eq!(file.sha256, sha256_bytes(&contents));
        }
    }

    #[test]
    fn scan_returns_empty_when_overlay_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = OverlayEngine::new(tmp.path().join("no-such-overlay"));
        assert!(engine.scan().unwrap().is_empty());
    }

    #[test]
    fn validate_overlay_path_rejects_unsafe_paths() {
        for bad in [
            "/etc/passwd",
            "../escape",
            "a/../b",
            "a/./b",
            "",
            "a\0b.jar",
        ] {
            assert!(
                validate_overlay_path(Path::new(bad)).is_err(),
                "expected rejection for {bad:?}"
            );
        }
        assert_eq!(
            validate_overlay_path(Path::new("a\\b.jar")).unwrap(),
            PathBuf::from("a/b.jar")
        );
    }

    #[test]
    fn apply_copies_new_files_and_creates_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay = tmp.path().join("overlay");
        write_file(&overlay, "mods/grieflogger.jar", b"GR");
        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();

        let engine = OverlayEngine::new(overlay.clone());
        let files = engine.scan().unwrap();
        let copied = engine.apply(&files, &server).unwrap();

        assert_eq!(copied, 1);
        assert_eq!(
            std::fs::read(server.join("mods/grieflogger.jar")).unwrap(),
            b"GR"
        );
    }

    #[test]
    fn apply_skips_unchanged_and_copies_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay = tmp.path().join("overlay");
        write_file(&overlay, "config/main.conf", b"cfg");
        let server = tmp.path().join("server");
        std::fs::create_dir_all(server.join("config")).unwrap();
        std::fs::write(server.join("config/main.conf"), b"cfg").unwrap();

        let engine = OverlayEngine::new(overlay.clone());
        let files = engine.scan().unwrap();
        assert_eq!(
            engine.apply(&files, &server).unwrap(),
            0,
            "identical content"
        );

        write_file(&overlay, "config/main.conf", b"NEWCFG");
        let files = engine.scan().unwrap();
        assert_eq!(engine.apply(&files, &server).unwrap(), 1, "changed content");
        assert_eq!(
            std::fs::read(server.join("config/main.conf")).unwrap(),
            b"NEWCFG"
        );
    }

    #[test]
    fn apply_rejects_traversal_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();

        let engine = OverlayEngine::new(tmp.path().join("overlay"));
        let escape = OverlayFile {
            rel_path: PathBuf::from("../escape"),
            source: tmp.path().join("src.txt"),
            sha256: String::new(),
            size: 0,
        };
        assert!(engine.apply(&[escape], &server).is_err());
    }

    #[test]
    fn apply_copies_into_nested_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay = tmp.path().join("overlay");
        write_file(&overlay, "config/MiniMOTD/main.conf", b"MM");
        write_file(&overlay, "kubejs/server_scripts/foo.js", b"JS");
        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();

        let engine = OverlayEngine::new(overlay.clone());
        let files = engine.scan().unwrap();
        let copied = engine.apply(&files, &server).unwrap();

        assert_eq!(copied, 2);
        assert_eq!(
            std::fs::read(server.join("config/MiniMOTD/main.conf")).unwrap(),
            b"MM"
        );
        assert_eq!(
            std::fs::read(server.join("kubejs/server_scripts/foo.js")).unwrap(),
            b"JS"
        );
    }
}
