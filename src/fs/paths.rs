//! Path safety helpers.
//!
//! Paths originating from archives, provider metadata, configuration files,
//! and overlays are treated as untrusted. Every such path must be validated
//! before it is used to touch the filesystem.

use std::path::{Path, PathBuf};

use crate::error::{PackError, Result};

/// Returns true when `rel` is a safe relative path.
///
/// A path is considered safe when it is non-empty, relative, contains no NUL
/// byte, and has no component equal to `.` or `..`. Both `/` and `\` are
/// treated as separators so Windows-style archive paths are handled correctly.
pub fn is_safe_relative(rel: &Path) -> bool {
    if rel.as_os_str().is_empty() || rel.is_absolute() {
        return false;
    }
    let as_str = rel.to_string_lossy();
    if as_str.contains('\0') {
        return false;
    }
    as_str
        .split(['/', '\\'])
        .all(|component| component != "." && component != "..")
}

/// Normalizes a relative path and rejects unsafe forms.
///
/// Backslashes are converted to forward slashes first. Absolute paths, `..`
/// components, `.` components, empty components, and NUL bytes are rejected.
/// Returns the cleaned relative path on success.
pub fn normalize_relative(rel: &Path) -> Result<PathBuf> {
    if rel.is_absolute() {
        return Err(PackError::UnsafePath(rel.to_path_buf()));
    }
    let as_str = rel.to_string_lossy();
    if as_str.contains('\0') {
        return Err(PackError::UnsafePath(rel.to_path_buf()));
    }
    let normalized = as_str.replace('\\', "/");
    for component in normalized.split('/') {
        if matches!(component, "" | "." | "..") {
            return Err(PackError::UnsafePathComponent {
                path: rel.to_path_buf(),
                component: component.to_string(),
            });
        }
    }
    Ok(PathBuf::from(normalized))
}

/// Joins `rel` onto `root` after validating that `rel` is safe.
///
/// The containment check is purely lexical; it does not resolve symlinks.
/// Callers that must guard against symlink escapes are responsible for
/// resolving links before relying on the result.
pub fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf> {
    let clean = normalize_relative(rel)?;
    let joined = root.join(clean);
    if !is_within(root, &joined) {
        return Err(PackError::UnsafePath(rel.to_path_buf()));
    }
    Ok(joined)
}

/// Returns true when `candidate` is `root` itself or lexically inside `root`.
///
/// This is a lexical comparison only; symlinks are not resolved.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_ok_and(|rest| !rest.as_os_str().is_empty())
}

/// Returns the relative path of `full` beneath `root`.
///
/// Errors when `full` is not lexically inside `root`.
pub fn strip_server_root(root: &Path, full: &Path) -> Result<PathBuf> {
    full.strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| PackError::Path {
            message: format!("path is not inside the server root '{}'", root.display()),
            path: full.to_path_buf(),
        })
}

/// Returns true when `path` has a `jar` extension, case-insensitively.
pub fn is_jar_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_relative_accepts_valid_paths() {
        assert_eq!(
            normalize_relative(Path::new("mods/foo.jar")).unwrap(),
            PathBuf::from("mods/foo.jar")
        );
        assert_eq!(
            normalize_relative(Path::new("a/b/c")).unwrap(),
            PathBuf::from("a/b/c")
        );
        assert_eq!(
            normalize_relative(Path::new("a\\b\\c.jar")).unwrap(),
            PathBuf::from("a/b/c.jar")
        );
    }

    #[test]
    fn normalize_relative_rejects_unsafe_paths() {
        assert!(normalize_relative(Path::new("/etc/passwd")).is_err());
        assert!(normalize_relative(Path::new("../x")).is_err());
        assert!(normalize_relative(Path::new("a/../b")).is_err());
        assert!(normalize_relative(Path::new("x/./y")).is_err());
        assert!(normalize_relative(Path::new("")).is_err());
        assert!(normalize_relative(Path::new("a\0b.jar")).is_err());
    }

    #[test]
    fn safe_join_joins_and_rejects_escapes() {
        let root = Path::new("/srv/mc");
        assert_eq!(
            safe_join(root, Path::new("mods/foo.jar")).unwrap(),
            PathBuf::from("/srv/mc/mods/foo.jar")
        );
        assert_eq!(
            safe_join(root, Path::new("a\\b.jar")).unwrap(),
            PathBuf::from("/srv/mc/a/b.jar")
        );
        assert!(safe_join(root, Path::new("../escape")).is_err());
        assert!(safe_join(root, Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn strip_server_root_works_and_errors_when_outside() {
        let root = Path::new("/srv/mc");
        assert_eq!(
            strip_server_root(root, Path::new("/srv/mc/mods/foo.jar")).unwrap(),
            PathBuf::from("mods/foo.jar")
        );
        assert!(strip_server_root(root, Path::new("/elsewhere/foo.jar")).is_err());
    }

    #[test]
    fn is_jar_path_is_case_insensitive() {
        assert!(is_jar_path(Path::new("mods/foo.jar")));
        assert!(is_jar_path(Path::new("mods/foo.JAR")));
        assert!(is_jar_path(Path::new("mods/foo.JaR")));
        assert!(!is_jar_path(Path::new("mods/foo.txt")));
        assert!(!is_jar_path(Path::new("mods/foo")));
        assert!(!is_jar_path(Path::new("mods/foo.jar.bak")));
    }

    #[test]
    fn is_safe_relative_checks() {
        assert!(is_safe_relative(Path::new("mods/foo.jar")));
        assert!(is_safe_relative(Path::new("a\\b\\c.jar")));
        assert!(!is_safe_relative(Path::new("/etc/passwd")));
        assert!(!is_safe_relative(Path::new("../x")));
        assert!(!is_safe_relative(Path::new("a/./b")));
        assert!(!is_safe_relative(Path::new("a\0b")));
        assert!(!is_safe_relative(Path::new("")));
    }

    #[test]
    fn is_within_is_lexical() {
        let root = Path::new("/srv/mc");
        assert!(is_within(root, root));
        assert!(is_within(root, Path::new("/srv/mc/mods/foo.jar")));
        assert!(!is_within(root, Path::new("/srv/mc2/foo.jar")));
        assert!(!is_within(root, Path::new("/srv/foo.jar")));
    }
}
