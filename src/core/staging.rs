//! Staging directories for preparing upstream content.
//!
//! All downloads, extraction, and upstream preparation must happen in a
//! staging directory, never directly inside the live server (see design notes
//! "Never Update the Live Server Directly During Preparation"). Once
//! preparation succeeds, the planner builds the update plan from the staged
//! content and the executor applies it to the live server.
//!

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{PackError, Result};
use crate::fs::copy::remove_tree;
use crate::fs::paths::safe_join;

/// Monotonic counter used to keep staging directory names unique within a
/// process even when created within the same nanosecond.
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A packctl-owned temporary directory used to prepare upstream content
/// without touching the live server.
///
/// The root is a uniquely named directory
/// (`packctl-staging-<process-id>-<nanosecond timestamp>-<counter>`) created
/// under the system temp dir ([`StagingDir::create_default`]) or a
/// caller-supplied base ([`StagingDir::create_in`]).
///
/// Dropping the value removes the entire staging tree with `remove_tree`. The
/// drop only ever touches packctl-owned directories: the root was created by
/// this type, so removing it recursively is safe. Removing a root that was
/// already removed (for example by manual cleanup) is a no-op.
pub struct StagingDir {
    pub root: PathBuf,
}

impl StagingDir {
    /// Creates a unique staging directory under the system temp dir.
    pub fn create_default() -> Result<Self> {
        Self::create_in(&std::env::temp_dir())
    }

    /// Creates a unique staging directory directly under `base`.
    ///
    /// The directory name is unique per process and per call, so concurrently
    /// alive staging directories never collide.
    pub fn create_in(base: &Path) -> Result<Self> {
        let root = base.join(unique_staging_name());
        std::fs::create_dir_all(&root).map_err(|e| {
            PackError::io(format!("create staging directory '{}'", root.display()), e)
        })?;
        Ok(StagingDir { root })
    }

    /// Absolute path of the staging root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the absolute path of a named subdirectory inside the staging
    /// root, creating it (including nested parents) if needed.
    ///
    /// `name` is treated as untrusted and validated before joining; unsafe
    /// names (absolute, `..`, empty components, NUL bytes) are rejected.
    pub fn subdir(&self, name: &str) -> Result<PathBuf> {
        let path = safe_join(&self.root, Path::new(name))?;
        std::fs::create_dir_all(&path).map_err(|e| {
            PackError::io(
                format!("create staging subdirectory '{}'", path.display()),
                e,
            )
        })?;
        Ok(path)
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = remove_tree(&self.root);
    }
}

/// Builds a staging directory name unique across processes and calls.
///
/// The name embeds the process id, a nanosecond timestamp, and a monotonic
/// counter, so two staging directories created in the same process never
/// collide.
fn unique_staging_name() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        "packctl-staging-{}-{}-{}",
        std::process::id(),
        nanos,
        counter
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_default_creates_existing_dir_and_cleans_up_on_drop() {
        let root;
        {
            let staging = StagingDir::create_default().unwrap();
            root = staging.root().to_path_buf();
            assert!(root.is_dir(), "staging root must exist");
            std::fs::write(root.join("artifact.txt"), b"prepared").unwrap();
        }
        assert!(!root.exists(), "drop must remove the whole staging tree");
    }

    #[test]
    fn create_in_creates_under_given_base() {
        let base = tempfile::tempdir().unwrap();
        let root;
        {
            let staging = StagingDir::create_in(base.path()).unwrap();
            root = staging.root().to_path_buf();
            assert!(root.starts_with(base.path()), "must live under the base");
            assert!(root.is_dir());
        }
        assert!(!root.exists(), "drop must clean up under the base");
    }

    #[test]
    fn subdir_creates_nested_path_inside_root() {
        let staging = StagingDir::create_default().unwrap();
        let sub = staging.subdir("mods/extra").unwrap();
        assert!(sub.is_dir());
        assert!(sub.starts_with(staging.root()));
        assert_eq!(sub, staging.root().join("mods/extra"));
    }

    #[test]
    fn subdir_rejects_unsafe_names() {
        let staging = StagingDir::create_default().unwrap();
        assert!(staging.subdir("../escape").is_err());
        assert!(staging.subdir("/absolute").is_err());
    }

    #[test]
    fn two_staging_dirs_get_unique_roots() {
        let a = StagingDir::create_default().unwrap();
        let b = StagingDir::create_default().unwrap();
        assert_ne!(a.root(), b.root());
        let c = StagingDir::create_in(&std::env::temp_dir()).unwrap();
        assert_ne!(c.root(), a.root());
        assert_ne!(c.root(), b.root());
    }

    #[test]
    fn dropping_an_already_removed_root_is_fine() {
        let staging = StagingDir::create_default().unwrap();
        remove_tree(staging.root()).unwrap();
        drop(staging);
    }
}
