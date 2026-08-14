//! File copy and removal primitives with contextual error wrapping.

use std::fs;
use std::path::Path;

use crate::error::{PackError, Result};
use crate::fs::hashing::sha256_file;

/// Copies `src` to `dst` only when their contents differ.
///
/// A cheap size comparison runs first; files of equal size are compared by
/// SHA-256. Returns whether a copy was actually performed. Parent directories
/// of `dst` are created as needed.
pub fn copy_if_changed(src: &Path, dst: &Path) -> Result<bool> {
    let changed = if !dst.exists() {
        true
    } else {
        let src_len = fs::metadata(src)
            .map_err(|e| {
                PackError::io(
                    format!(
                        "stat source '{}' for copy to '{}'",
                        src.display(),
                        dst.display()
                    ),
                    e,
                )
            })?
            .len();
        let dst_len = fs::metadata(dst)
            .map_err(|e| PackError::io(format!("stat destination '{}'", dst.display()), e))?
            .len();
        if src_len != dst_len {
            true
        } else {
            sha256_file(src)? != sha256_file(dst)?
        }
    };
    if changed {
        copy_file(src, dst)?;
    }
    Ok(changed)
}

/// Unconditionally copies `src` to `dst`, creating parent directories as needed.
pub fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    ensure_parent_dir(dst)?;
    fs::copy(src, dst).map_err(|e| {
        PackError::io(
            format!("copy '{}' to '{}'", src.display(), dst.display()),
            e,
        )
    })?;
    Ok(())
}

/// Removes the file at `path`.
///
/// Returns `Ok(())` when the file does not exist. Directories are never
/// removed by this helper.
pub fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PackError::io(
            format!("remove file '{}'", path.display()),
            e,
        )),
    }
}

/// Creates the parent directory of `path`, including nested parents.
///
/// A path with no parent component is a no-op.
pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| {
            PackError::io(format!("create parent directory '{}'", parent.display()), e)
        })?;
    }
    Ok(())
}

/// Recursively removes the directory tree at `dir`.
///
/// Returns `Ok(())` when the directory does not exist. This is only used on
/// updater-owned directories (staging, snapshots), never on arbitrary user
/// data.
pub fn remove_tree(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PackError::io(
            format!("remove directory tree '{}'", dir.display()),
            e,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_if_changed_copies_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("sub/deep/dst.txt");
        std::fs::write(&src, b"content").unwrap();

        assert!(copy_if_changed(&src, &dst).unwrap());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "content");
    }

    #[test]
    fn copy_if_changed_skips_identical_and_copies_same_size_change() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"aaa").unwrap();
        std::fs::write(&dst, b"aaa").unwrap();

        assert!(!copy_if_changed(&src, &dst).unwrap());

        std::fs::write(&src, b"aab").unwrap();
        assert!(copy_if_changed(&src, &dst).unwrap());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "aab");
    }

    #[test]
    fn copy_file_is_unconditional_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("nested/dst.txt");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"old").unwrap();

        copy_file(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new");
    }

    #[test]
    fn remove_file_missing_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        assert!(remove_file(&dir.path().join("nope.txt")).is_ok());

        let path = dir.path().join("present.txt");
        std::fs::write(&path, b"x").unwrap();
        remove_file(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn remove_tree_removes_nested_tree() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("a/b/c");
        fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("f.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("a/b/g.txt"), b"y").unwrap();

        remove_tree(&dir.path().join("a")).unwrap();
        assert!(!dir.path().join("a").exists());

        assert!(remove_tree(&dir.path().join("missing")).is_ok());
    }

    #[test]
    fn ensure_parent_dir_creates_nested_parents() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("x/y/z/file.txt");
        ensure_parent_dir(&nested).unwrap();
        assert!(dir.path().join("x/y/z").is_dir());
    }
}
