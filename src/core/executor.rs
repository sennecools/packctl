//! Update executor.
//!
//! The executor applies an already-built [`UpdatePlan`] to the live server
//! root. It never plans; `packctl plan` and `packctl update` share the same
//! planner and the executor runs exactly the plan that was displayed (see
//! design notes "Update Executor" and "Plan once, execute that same plan").
//!
//! Mutations are ordered: removals run first, then additions and
//! modifications. The mirrored overlay is applied afterwards by the
//! [`OverlayEngine`], so overlay content always wins over upstream content.
//!
// The public API in this module is consumed by the CLI modules, which are not
// implemented yet. Allow dead_code so the ready API surface does not emit
// warnings while the rest of the core is being built out.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::core::overlay::{OverlayEngine, OverlayFile};
use crate::core::planner::{FileChange, UpdatePlan};
use crate::error::{PackError, Result};
use crate::fs::copy::{copy_if_changed, remove_file};
use crate::fs::paths::{is_within, safe_join};

/// Applies a built [`UpdatePlan`] to the live server root.
pub struct UpdateExecutor {
    pub server_root: PathBuf,
}

impl UpdateExecutor {
    pub fn new(server_root: PathBuf) -> Self {
        UpdateExecutor { server_root }
    }

    /// Applies the upstream (non-overlay) portion of `plan`.
    ///
    /// Removals run first so a managed file dropped by the new upstream
    /// version is gone before any new content is written. A missing removal
    /// target is not an error (the file may already be absent).
    ///
    /// Additions and modifications copy the staged source into the server root
    /// via [`copy_if_changed`], creating parent directories as needed and
    /// skipping files whose content is already identical. Every non-removal
    /// change must record a source and that source must resolve inside
    /// `staged_root`; both are enforced with contextual errors. Returns how
    /// many files were actually written.
    pub fn apply_plan(&self, plan: &UpdatePlan, staged_root: &Path) -> Result<usize> {
        let mut writes = 0;

        for change in &plan.removals {
            let dest = safe_join(&self.server_root, &change.rel_path)?;
            remove_file(&dest)?;
        }

        for change in plan.additions.iter().chain(plan.modifications.iter()) {
            let source = staged_source(change, staged_root)?;
            let dest = safe_join(&self.server_root, &change.rel_path)?;
            if copy_if_changed(source, &dest).map_err(|err| write_error(&change.rel_path, err))? {
                writes += 1;
            }
        }

        Ok(writes)
    }

    /// Applies the mirrored overlay over the server root.
    ///
    /// Delegates to [`OverlayEngine::apply`]; failures are re-wrapped with the
    /// affected relative path so the message stays actionable.
    pub fn apply_overlay(&self, overlay: &OverlayEngine, files: &[OverlayFile]) -> Result<usize> {
        overlay
            .apply(files, &self.server_root)
            .map_err(|err| overlay_error(err, &self.server_root))
    }
}

/// Returns the staged source for a non-removal change, enforcing that it is
/// recorded and that it resolves inside the staging root.
fn staged_source<'a>(change: &'a FileChange, staged_root: &Path) -> Result<&'a Path> {
    let source = change
        .source
        .as_deref()
        .ok_or_else(|| missing_source_error(change))?;
    if !is_within(staged_root, source) {
        return Err(PackError::Path {
            message: format!(
                "Update failed while applying the upstream changes.\n\nCould not write:\n  {}\n\nReason:\n  source '{}' is not inside the staging root '{}'",
                change.rel_path.display(),
                source.display(),
                staged_root.display()
            ),
            path: change.rel_path.clone(),
        });
    }
    Ok(source)
}

/// Builds the contextual error for a change that has no recorded source file.
fn missing_source_error(change: &FileChange) -> PackError {
    PackError::Path {
        message: format!(
            "Update failed while applying the upstream changes.\n\nCould not write:\n  {}\n\nReason:\n  no source file was recorded for this change",
            change.rel_path.display()
        ),
        path: change.rel_path.clone(),
    }
}

/// Wraps a copy failure with the affected relative path.
fn write_error(rel: &Path, error: PackError) -> PackError {
    PackError::Path {
        message: format!(
            "Update failed while applying the upstream changes.\n\nCould not write:\n  {}\n\nReason:\n  {error}",
            rel.display()
        ),
        path: rel.to_path_buf(),
    }
}

/// Wraps an overlay-apply failure with the affected relative path.
///
/// The overlay engine reports copy failures with the resolved destination
/// embedded in the operation text; this recovers the relative path so the
/// failure stays actionable (see design notes "Error Messages").
fn overlay_error(error: PackError, server_root: &Path) -> PackError {
    let rel = overlay_failed_rel(&error, server_root);
    let rel_text = rel
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    PackError::Path {
        message: format!(
            "Update failed while applying the overlay.\n\nCould not write:\n  {rel_text}\n\nReason:\n  {error}"
        ),
        path: rel.unwrap_or_else(|| PathBuf::from("<unknown>")),
    }
}

/// Recovers the destination relative path from an overlay-apply error.
fn overlay_failed_rel(error: &PackError, server_root: &Path) -> Option<PathBuf> {
    match error {
        // Unsafe destination paths carry the untrusted relative path directly.
        PackError::UnsafePath(rel) | PackError::UnsafePathComponent { path: rel, .. } => {
            Some(rel.clone())
        }
        // Copy/stat failures embed the resolved destination in the operation.
        PackError::Io { operation, .. } => {
            let marker = "(applying overlay to '";
            let rest = &operation[operation.rfind(marker)? + marker.len()..];
            let dest = &rest[..rest.find("')")?];
            Some(
                Path::new(dest)
                    .strip_prefix(server_root)
                    .ok()?
                    .to_path_buf(),
            )
        }
        PackError::Path { path, .. } => path.strip_prefix(server_root).ok().map(Path::to_path_buf),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::planner::ChangeKind;

    fn write(path: &Path, data: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    fn change(rel: &str, kind: ChangeKind, source: Option<PathBuf>) -> FileChange {
        FileChange {
            rel_path: PathBuf::from(rel),
            kind,
            source,
            sha256: None,
        }
    }

    fn plan(
        additions: Vec<FileChange>,
        modifications: Vec<FileChange>,
        removals: Vec<FileChange>,
    ) -> UpdatePlan {
        UpdatePlan {
            from_version: None,
            from_id: None,
            to_version: "to".to_string(),
            to_id: "id".to_string(),
            additions,
            modifications,
            removals,
            overlay_changes: Vec::new(),
            notices: Vec::new(),
        }
    }

    #[test]
    fn apply_plan_adds_replaces_removes_and_creates_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();
        write(&server.join("config/settings.toml"), b"old");
        write(&server.join("mods/removed.jar"), b"bye");

        let staging = tmp.path().join("staging");
        write(&staging.join("config/settings.toml"), b"new");
        write(&staging.join("mods/new.jar"), b"new jar");

        let p = plan(
            vec![change(
                "mods/new.jar",
                ChangeKind::Add,
                Some(staging.join("mods/new.jar")),
            )],
            vec![change(
                "config/settings.toml",
                ChangeKind::Replace,
                Some(staging.join("config/settings.toml")),
            )],
            vec![change("mods/removed.jar", ChangeKind::Remove, None)],
        );

        let writes = UpdateExecutor::new(server.clone())
            .apply_plan(&p, &staging)
            .unwrap();

        assert_eq!(writes, 2);
        assert_eq!(
            std::fs::read_to_string(server.join("config/settings.toml")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(server.join("mods/new.jar")).unwrap(),
            "new jar"
        );
        assert!(!server.join("mods/removed.jar").exists());
    }

    #[test]
    fn apply_plan_leaves_persistent_world_and_unknown_files_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        write(&server.join("world/region/r.mca"), b"world data");
        write(&server.join("local-only/custom.jar"), b"custom");

        let staging = tmp.path().join("staging");
        write(&staging.join("mods/upstream.jar"), b"upstream");

        let p = plan(
            vec![change(
                "mods/upstream.jar",
                ChangeKind::Add,
                Some(staging.join("mods/upstream.jar")),
            )],
            Vec::new(),
            Vec::new(),
        );

        let writes = UpdateExecutor::new(server.clone())
            .apply_plan(&p, &staging)
            .unwrap();
        assert_eq!(writes, 1);
        assert_eq!(
            std::fs::read_to_string(server.join("world/region/r.mca")).unwrap(),
            "world data"
        );
        assert_eq!(
            std::fs::read_to_string(server.join("local-only/custom.jar")).unwrap(),
            "custom"
        );
    }

    #[test]
    fn apply_plan_removes_only_planned_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();
        write(&server.join("mods/a.jar"), b"a");
        write(&server.join("mods/b.jar"), b"b");
        write(&server.join("mods/c.jar"), b"c");

        let p = plan(
            Vec::new(),
            Vec::new(),
            vec![
                change("mods/a.jar", ChangeKind::Remove, None),
                change("mods/b.jar", ChangeKind::Remove, None),
            ],
        );

        UpdateExecutor::new(server.clone())
            .apply_plan(&p, tmp.path())
            .unwrap();

        assert!(!server.join("mods/a.jar").exists());
        assert!(!server.join("mods/b.jar").exists());
        assert!(server.join("mods/c.jar").exists());
    }

    #[test]
    fn apply_plan_does_not_rewrite_identical_content() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        write(&server.join("config/same.toml"), b"identical");

        let staging = tmp.path().join("staging");
        write(&staging.join("config/same.toml"), b"identical");

        let p = plan(
            vec![change(
                "config/same.toml",
                ChangeKind::Replace,
                Some(staging.join("config/same.toml")),
            )],
            Vec::new(),
            Vec::new(),
        );

        let writes = UpdateExecutor::new(server)
            .apply_plan(&p, &staging)
            .unwrap();
        assert_eq!(writes, 0);
    }

    #[test]
    fn apply_plan_missing_source_errors_with_rel_path() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();

        let p = plan(
            vec![change("mods/ghost.jar", ChangeKind::Add, None)],
            Vec::new(),
            Vec::new(),
        );

        let err = UpdateExecutor::new(server)
            .apply_plan(&p, tmp.path())
            .unwrap_err();
        match err {
            PackError::Path { message, path } => {
                assert!(message.contains("mods/ghost.jar"), "message: {message}");
                assert_eq!(path, PathBuf::from("mods/ghost.jar"));
            }
            other => panic!("expected Path error, got {other:?}"),
        }
    }

    #[test]
    fn apply_plan_rejects_source_outside_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();
        let outside = tmp.path().join("elsewhere/file.txt");
        write(&outside, b"x");

        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let p = plan(
            vec![change("file.txt", ChangeKind::Add, Some(outside))],
            Vec::new(),
            Vec::new(),
        );

        assert!(
            UpdateExecutor::new(server)
                .apply_plan(&p, &staging)
                .is_err()
        );
    }

    #[test]
    fn apply_overlay_copies_overlay_files() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay_dir = tmp.path().join("overlay");
        write(&overlay_dir.join("mods/grieflogger.jar"), b"GR");
        write(&overlay_dir.join("server.properties"), b"props");

        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();

        let engine = OverlayEngine::new(overlay_dir.clone());
        let files = engine.scan().unwrap();
        let copied = UpdateExecutor::new(server.clone())
            .apply_overlay(&engine, &files)
            .unwrap();

        assert_eq!(copied, 2);
        assert_eq!(
            std::fs::read_to_string(server.join("mods/grieflogger.jar")).unwrap(),
            "GR"
        );
        assert_eq!(
            std::fs::read_to_string(server.join("server.properties")).unwrap(),
            "props"
        );
    }

    #[test]
    fn apply_overlay_error_message_contains_rel_path() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir_all(&server).unwrap();

        let engine = OverlayEngine::new(tmp.path().join("overlay"));
        let missing = OverlayFile {
            rel_path: PathBuf::from("mods/ghost.jar"),
            source: tmp.path().join("no-such-source.jar"),
            sha256: String::new(),
            size: 0,
        };

        let err = UpdateExecutor::new(server)
            .apply_overlay(&engine, &[missing])
            .unwrap_err();
        match err {
            PackError::Path { message, .. } => {
                assert!(message.contains("mods/ghost.jar"), "message: {message}");
                assert!(
                    message.contains("applying the overlay"),
                    "message: {message}"
                );
            }
            other => panic!("expected Path error, got {other:?}"),
        }
    }
}
