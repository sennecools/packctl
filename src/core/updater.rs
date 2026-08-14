//! Update orchestration.
//!
//! The [`Updater`] drives the full update lifecycle in the exact order mandated
//! by design notes "Fundamental Update Model": stop the server, snapshot, apply
//! upstream changes, apply the overlay, validate, start the server, and only
//! then commit the new state. It never mutates the live server during
//! preparation, and a failed mutation always surfaces a rollback pointer so an
//! administrator can recover the previous version (see design notes "Rollback" and
//! "Error Messages").
//!
//! Preparation (`prepare_update`) is pure and safe: it resolves a version,
//! stages the upstream pack, scans the overlay, and builds the exact
//! [`UpdatePlan`] shared with `packctl plan`. Execution (`execute`) is the
//! single place that stops the server and mutates managed files.
//!

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::config::profile::{ProviderKind, ServerProfile};
use crate::controllers::ServerController;
use crate::core::executor::UpdateExecutor;
use crate::core::overlay::{OverlayEngine, OverlayFile};
use crate::core::ownership::FilePolicy;
use crate::core::planner::{UpdatePlan, UpdatePlanner};
use crate::core::snapshot::{Snapshot, create_snapshot};
use crate::core::staging::StagingDir;
use crate::core::state::{InstalledState, ManagedFile, StateStore};
use crate::core::validation::{Severity, has_errors, validate};
use crate::error::{PackError, Result};
use crate::providers::curseforge::client::CfClient;
use crate::providers::curseforge::installer::CurseForgeProvider;
use crate::providers::{PackProvider, PackRef, PreparedPack, VersionSelector};

/// Everything needed to run a single planned update.
pub struct PreparedUpdate {
    pub overlay_files: Vec<OverlayFile>,
    pub plan: UpdatePlan,
    pub prepared: PreparedPack,
    /// Held so the staged sources remain valid for the duration of execution.
    pub staging: StagingDir,
}

/// The result of executing an update.
#[derive(Debug)]
pub struct UpdateOutcome {
    pub upstream_writes: usize,
    pub overlay_copied: usize,
    pub snapshot: Option<Snapshot>,
    pub committed: bool,
}

/// Orchestrates a full update for one server profile.
pub struct Updater {
    pub profile: ServerProfile,
    pub provider: Box<dyn PackProvider>,
    pub controller: Box<dyn ServerController>,
}

impl Updater {
    /// Build an updater from explicit provider and controller implementations.
    pub fn new(
        profile: ServerProfile,
        provider: Box<dyn PackProvider>,
        controller: Box<dyn ServerController>,
    ) -> Self {
        Updater {
            profile,
            provider,
            controller,
        }
    }

    /// Build an updater from a profile's configured provider and controller.
    ///
    /// Only CurseForge packs are supported in V1; any other provider is a
    /// configuration error.
    pub fn from_profile(profile: &ServerProfile) -> Result<Self> {
        if profile.pack.provider != ProviderKind::CurseForge {
            return Err(PackError::Config(format!(
                "unsupported pack provider {:?}: only CurseForge is supported",
                profile.pack.provider
            )));
        }
        let provider: Box<dyn PackProvider> = Box::new(CurseForgeProvider::new(
            CfClient::with_api_key(profile.curseforge_api_key()?),
        ));
        let controller = crate::controllers::from_profile(&profile.controller)?;
        Ok(Updater {
            profile: profile.clone(),
            provider,
            controller,
        })
    }

    /// The upstream pack this profile follows.
    pub fn pack_ref(&self) -> PackRef {
        PackRef {
            project_id: self.profile.pack.project_id,
            slug: self.profile.pack.slug.clone().unwrap_or_default(),
        }
    }

    /// Loads the installed state recorded under the server root.
    pub fn load_state(&self) -> Result<InstalledState> {
        StateStore::at(&self.profile.server.root)?.load()
    }

    /// Prepares an update without touching the live server.
    ///
    /// Resolves the requested version, stages the upstream pack, scans the
    /// overlay, and builds the exact [`UpdatePlan`] that `execute` will run.
    pub async fn prepare_update(&self, selector: &VersionSelector) -> Result<PreparedUpdate> {
        let state = self.load_state()?;
        let resolved = self
            .provider
            .resolve_version(&self.pack_ref(), selector)
            .await?;
        let staging = StagingDir::create_default()?;
        let prepared = self.provider.prepare(&resolved, &staging.root).await?;
        let overlay_files = OverlayEngine::new(self.profile.overlay.path.clone()).scan()?;
        let plan = UpdatePlanner::new(FilePolicy::default_policy()).build_plan(
            &state,
            &prepared,
            &overlay_files,
        )?;
        Ok(PreparedUpdate {
            overlay_files,
            plan,
            prepared,
            staging,
        })
    }

    /// Executes a prepared update against the live server.
    ///
    /// Follows design notes "Fundamental Update Model" exactly: an empty plan is a
    /// no-op; otherwise the server is stopped, a rollback snapshot is created,
    /// upstream changes and then the overlay are applied, the result is
    /// validated, the server is started, and only after all of that succeeds is
    /// the new state committed. Every mutation-phase failure after the snapshot
    /// exists carries the snapshot location so the previous version can be
    /// restored.
    pub async fn execute(&self, prepared: &PreparedUpdate) -> Result<UpdateOutcome> {
        let plan = prepared.plan.clone();
        let server_root = &self.profile.server.root;

        if plan.is_empty() {
            return Ok(UpdateOutcome {
                upstream_writes: 0,
                overlay_copied: 0,
                snapshot: None,
                committed: false,
            });
        }

        self.controller.stop().await?;

        let snapshot = snapshot_before_mutation(&plan, server_root)?;

        let executor = UpdateExecutor::new(server_root.clone());
        let upstream_writes = executor
            .apply_plan(&plan, &prepared.prepared.root)
            .map_err(|error| snapshot_context(&snapshot, error))?;

        let overlay_copied = executor
            .apply_overlay(
                &OverlayEngine::new(self.profile.overlay.path.clone()),
                &prepared.overlay_files,
            )
            .map_err(|error| snapshot_context(&snapshot, error))?;

        let issues = validate(
            &self.profile,
            Some(&prepared.prepared),
            &prepared.overlay_files,
            self.controller.as_ref(),
        )
        .await?;
        if has_errors(&issues) {
            let error_messages: Vec<String> = issues
                .iter()
                .filter(|issue| issue.severity == Severity::Error)
                .map(|issue| issue.message.clone())
                .collect();
            return Err(PackError::Validation(format!(
                "{}\nThe new version was not committed.\nRollback snapshot:\n  {}",
                error_messages.join("\n"),
                snapshot.dir.display()
            )));
        }

        self.controller.start().await?;

        let managed_files = managed_files_from_prepared(
            &FilePolicy::default_policy(),
            &prepared.prepared,
            &prepared.overlay_files,
        );
        let new_state = InstalledState {
            installed_version: Some(plan.to_version.clone()),
            provider_version_id: Some(plan.to_id.clone()),
            managed_files,
            last_successful_update: Some(Utc::now()),
        };
        StateStore::at(server_root)?.save(&new_state)?;

        Ok(UpdateOutcome {
            upstream_writes,
            overlay_copied,
            snapshot: Some(snapshot),
            committed: true,
        })
    }
}

/// Relative path of the state file inside the server root.
const STATE_REL_PATH: &str = ".packctl/state.json";

/// Creates the rollback snapshot covering every path the plan touches.
///
/// Only paths that already exist on disk are copied into the snapshot; planned
/// additions that do not exist yet are still recorded as tracked so a rollback
/// can remove them again. The pre-update `state.json` is included so a
/// rollback can also restore the previous state metadata (design notes "Rollback",
/// step "Restore state metadata").
fn snapshot_before_mutation(plan: &UpdatePlan, server_root: &Path) -> Result<Snapshot> {
    let mut tracked: Vec<PathBuf> = plan
        .additions
        .iter()
        .chain(plan.modifications.iter())
        .chain(plan.removals.iter())
        .map(|change| change.rel_path.clone())
        .collect();
    tracked.extend(
        plan.overlay_changes
            .iter()
            .map(|change| change.rel_path.clone()),
    );
    tracked.push(PathBuf::from(STATE_REL_PATH));

    let tracked_strs: Vec<String> = tracked.iter().map(|rel| rel_key(rel)).collect();

    let files: Vec<PathBuf> = tracked
        .iter()
        .filter(|rel| server_root.join(rel).exists())
        .map(|rel| server_root.join(rel))
        .collect();
    let files_refs: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    let tracked_strs_refs: Vec<&str> = tracked_strs.iter().map(String::as_str).collect();

    create_snapshot(server_root, &files_refs, &tracked_strs_refs)
}

/// Builds the managed-file set to commit for a prepared upstream pack.
///
/// Persistent runtime data and paths provided by the overlay are never recorded
/// as upstream-managed: persistent content must not be replaced merely because
/// a new pack version lacks it, and overlay content is not tracked in upstream
/// state because the overlay owns that path (see design notes "Managed File" and
/// "Overlay Wins").
fn managed_files_from_prepared(
    policy: &FilePolicy,
    prepared: &PreparedPack,
    overlay: &[OverlayFile],
) -> HashMap<String, ManagedFile> {
    let overlay_paths: HashSet<String> =
        overlay.iter().map(|file| rel_key(&file.rel_path)).collect();

    let mut managed = HashMap::new();
    for file in &prepared.files {
        if policy.is_persistent(&file.rel_path) {
            continue;
        }
        let key = rel_key(&file.rel_path);
        if overlay_paths.contains(&key) {
            continue;
        }
        managed.insert(
            key,
            ManagedFile {
                sha256: file.sha256.clone(),
                size: file.size,
            },
        );
    }
    managed
}

/// Converts a relative path to the forward-slash string key used by managed
/// state (mirrors the planner's key encoding).
fn rel_key(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Attaches the rollback-snapshot location to a mutation-phase failure.
///
/// Keeps the error actionable per design notes "Error Messages": once a mutation
/// has begun, a failed update must always point at the snapshot that restores
/// the previous version.
fn snapshot_context(snapshot: &Snapshot, cause: PackError) -> PackError {
    PackError::Other(format!(
        "{cause}\nThe new version was not committed.\nRollback snapshot:\n  {}",
        snapshot.dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::profile::{
        CommandConfig, ControllerKind, ControllerSection, OverlaySection, PackSection,
        ServerSection,
    };
    use crate::controllers::ServerStatus;
    use crate::fs::hashing::sha256_bytes;
    use crate::providers::{PackVersion, PreparedFile, ResolvedPackVersion};

    /// A fake controller that records lifecycle calls and its current state.
    #[derive(Clone)]
    struct FakeController {
        status: Arc<Mutex<ServerStatus>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl FakeController {
        fn new() -> Self {
            FakeController {
                status: Arc::new(Mutex::new(ServerStatus::Running)),
                log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn status_value(&self) -> ServerStatus {
            *self.status.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl ServerController for FakeController {
        async fn status(&self) -> Result<ServerStatus> {
            Ok(*self.status.lock().unwrap())
        }

        async fn stop(&self) -> Result<()> {
            self.log.lock().unwrap().push("stop".to_string());
            *self.status.lock().unwrap() = ServerStatus::Stopped;
            Ok(())
        }

        async fn start(&self) -> Result<()> {
            self.log.lock().unwrap().push("start".to_string());
            *self.status.lock().unwrap() = ServerStatus::Running;
            Ok(())
        }
    }

    /// A controller whose status probe always fails (unreachable), used to
    /// trigger validation errors. `stop`/`start` succeed unless `fail_stop` is
    /// set via [`FakeBrokenController::new_failing_stop`].
    struct FakeBrokenController {
        fail_stop: bool,
    }

    impl FakeBrokenController {
        fn new() -> Self {
            FakeBrokenController { fail_stop: false }
        }

        fn new_failing_stop() -> Self {
            FakeBrokenController { fail_stop: true }
        }
    }

    #[async_trait::async_trait]
    impl ServerController for FakeBrokenController {
        async fn status(&self) -> Result<ServerStatus> {
            Err(PackError::Controller("unreachable".to_string()))
        }

        async fn stop(&self) -> Result<()> {
            if self.fail_stop {
                Err(PackError::Controller("stop failed".to_string()))
            } else {
                Ok(())
            }
        }

        async fn start(&self) -> Result<()> {
            Ok(())
        }
    }

    /// A provider that stages two upstream files, `mods/upstream.jar` and
    /// `config/upstream.toml`, and resolves a single fixture version.
    struct FakeProvider;

    impl FakeProvider {
        fn version() -> PackVersion {
            PackVersion {
                id: "v2".to_string(),
                name: "2.0".to_string(),
                file_id: None,
                released: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl PackProvider for FakeProvider {
        async fn list_versions(&self, _pack: &PackRef) -> Result<Vec<PackVersion>> {
            Ok(vec![Self::version()])
        }

        async fn resolve_version(
            &self,
            pack: &PackRef,
            selector: &VersionSelector,
        ) -> Result<ResolvedPackVersion> {
            match selector {
                VersionSelector::Latest => Ok(ResolvedPackVersion {
                    pack: pack.clone(),
                    version: Self::version(),
                }),
                other => Err(PackError::Provider(format!(
                    "fake provider only supports Latest, got {other:?}"
                ))),
            }
        }

        async fn prepare(
            &self,
            version: &ResolvedPackVersion,
            staging: &Path,
        ) -> Result<PreparedPack> {
            let server_root = staging.join("server");
            std::fs::create_dir_all(server_root.join("mods")).unwrap();
            std::fs::create_dir_all(server_root.join("config")).unwrap();
            std::fs::write(server_root.join("mods/upstream.jar"), b"UP").unwrap();
            std::fs::write(server_root.join("config/upstream.toml"), b"TOML").unwrap();
            Ok(PreparedPack {
                name: "fixture-pack".to_string(),
                version: version.version.clone(),
                root: server_root,
                files: vec![
                    PreparedFile {
                        rel_path: PathBuf::from("mods/upstream.jar"),
                        size: 2,
                        sha256: sha256_bytes(b"UP"),
                    },
                    PreparedFile {
                        rel_path: PathBuf::from("config/upstream.toml"),
                        size: 4,
                        sha256: sha256_bytes(b"TOML"),
                    },
                ],
            })
        }
    }

    /// A provider that stages nothing, so the update plan is always empty.
    struct EmptyFakeProvider;

    #[async_trait::async_trait]
    impl PackProvider for EmptyFakeProvider {
        async fn list_versions(&self, _pack: &PackRef) -> Result<Vec<PackVersion>> {
            Ok(vec![FakeProvider::version()])
        }

        async fn resolve_version(
            &self,
            pack: &PackRef,
            _selector: &VersionSelector,
        ) -> Result<ResolvedPackVersion> {
            Ok(ResolvedPackVersion {
                pack: pack.clone(),
                version: FakeProvider::version(),
            })
        }

        async fn prepare(
            &self,
            version: &ResolvedPackVersion,
            staging: &Path,
        ) -> Result<PreparedPack> {
            let server_root = staging.join("server");
            std::fs::create_dir_all(&server_root).unwrap();
            Ok(PreparedPack {
                name: "fixture-pack".to_string(),
                version: version.version.clone(),
                root: server_root,
                files: Vec::new(),
            })
        }
    }

    fn profile(server_root: &Path, overlay: &Path) -> ServerProfile {
        ServerProfile {
            name: "test-server".to_string(),
            server: ServerSection {
                root: server_root.to_path_buf(),
            },
            pack: PackSection {
                provider: ProviderKind::CurseForge,
                project_id: 42,
                slug: None,
            },
            overlay: OverlaySection {
                path: overlay.to_path_buf(),
            },
            controller: ControllerSection {
                kind: ControllerKind::Command,
                instance: None,
                command: Some(CommandConfig {
                    status: vec!["true".to_string()],
                    stop: vec!["true".to_string()],
                    start: vec!["true".to_string()],
                    timeout_ms: None,
                }),
            },
            secrets: crate::config::profile::SecretsSection::default(),
        }
    }

    fn write_state(server_root: &Path, state: &InstalledState) {
        let path = server_root.join(".packctl").join("state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(state).unwrap()).unwrap();
    }

    fn snapshot_count(server_root: &Path) -> usize {
        let snapshots = server_root.join(".packctl").join("snapshots");
        std::fs::read_dir(&snapshots)
            .map(|entries| entries.flatten().count())
            .unwrap_or(0)
    }

    fn load_state(server_root: &Path) -> InstalledState {
        let path = server_root.join(".packctl").join("state.json");
        let bytes = std::fs::read(&path).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn happy_path_applies_upstream_and_commits_state() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("world/region")).unwrap();
        std::fs::write(server_root.join("custom-kept.txt"), b"keep me").unwrap();
        std::fs::write(server_root.join("world/region/r.mca"), b"world data").unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let controller = FakeController::new();
        let updater = Updater::new(
            profile,
            Box::new(FakeProvider),
            Box::new(controller.clone()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        let outcome = updater.execute(&prepared).await.unwrap();

        assert!(outcome.committed);
        assert_eq!(outcome.upstream_writes, 2);
        assert_eq!(outcome.overlay_copied, 0);
        assert!(outcome.snapshot.is_some());

        assert_eq!(
            std::fs::read(server_root.join("mods/upstream.jar")).unwrap(),
            b"UP"
        );
        assert_eq!(
            std::fs::read(server_root.join("config/upstream.toml")).unwrap(),
            b"TOML"
        );
        assert_eq!(
            std::fs::read(server_root.join("custom-kept.txt")).unwrap(),
            b"keep me"
        );
        assert_eq!(
            std::fs::read(server_root.join("world/region/r.mca")).unwrap(),
            b"world data"
        );

        let state = load_state(&server_root);
        assert_eq!(state.installed_version.as_deref(), Some("2.0"));
        assert_eq!(state.provider_version_id.as_deref(), Some("v2"));
        assert!(state.managed_files.contains_key("mods/upstream.jar"));
        assert!(state.managed_files.contains_key("config/upstream.toml"));
        assert!(!state.managed_files.contains_key("custom-kept.txt"));
        assert!(state.last_successful_update.is_some());

        assert_eq!(snapshot_count(&server_root), 1);
        assert_eq!(controller.log(), &["stop", "start"]);
        assert_eq!(controller.status_value(), ServerStatus::Running);
    }

    #[tokio::test]
    async fn failed_stop_does_not_commit_or_write_files() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let controller = FakeBrokenController::new_failing_stop();
        let updater = Updater::new(profile, Box::new(FakeProvider), Box::new(controller));

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        let err = updater.execute(&prepared).await.unwrap_err();
        assert!(
            matches!(err, PackError::Controller(_)),
            "expected Controller error, got {err:?}"
        );

        assert!(!server_root.join(".packctl").exists());
        assert!(!server_root.join("mods/upstream.jar").exists());
        assert!(!server_root.join("config/upstream.toml").exists());
    }

    #[tokio::test]
    async fn overlay_conflict_replaces_upstream_and_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();

        let state = InstalledState {
            installed_version: Some("1.0".to_string()),
            provider_version_id: Some("v1".to_string()),
            managed_files: [(
                "config/upstream.toml".to_string(),
                ManagedFile {
                    sha256: sha256_bytes(b"old-content"),
                    size: 11,
                },
            )]
            .into_iter()
            .collect(),
            ..InstalledState::default()
        };
        write_state(&server_root, &state);

        let overlay = tmp.path().join("overlay");
        std::fs::create_dir_all(overlay.join("config")).unwrap();
        std::fs::write(overlay.join("config/upstream.toml"), b"OVERLAY").unwrap();

        let profile = profile(&server_root, &overlay);
        let controller = FakeController::new();
        let updater = Updater::new(profile, Box::new(FakeProvider), Box::new(controller));

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(!prepared.plan.is_empty());
        let outcome = updater.execute(&prepared).await.unwrap();

        assert!(outcome.committed);
        assert!(outcome.snapshot.is_some());
        assert_eq!(
            std::fs::read_to_string(server_root.join("config/upstream.toml")).unwrap(),
            "OVERLAY",
            "overlay content must win over upstream"
        );

        let saved = load_state(&server_root);
        assert!(
            !saved.managed_files.contains_key("config/upstream.toml"),
            "overlay-provided paths must not be recorded as upstream-managed"
        );
        assert!(saved.managed_files.contains_key("mods/upstream.jar"));
    }

    #[tokio::test]
    async fn empty_plan_skips_execution_without_contacting_server() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let controller = FakeController::new();
        let updater = Updater::new(
            profile,
            Box::new(EmptyFakeProvider),
            Box::new(controller.clone()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(prepared.plan.is_empty());

        let outcome = updater.execute(&prepared).await.unwrap();
        assert!(!outcome.committed);
        assert_eq!(outcome.upstream_writes, 0);
        assert_eq!(outcome.overlay_copied, 0);
        assert!(outcome.snapshot.is_none());
        assert!(controller.log().is_empty());
        assert!(!server_root.join(".packctl").exists());
    }

    #[tokio::test]
    async fn validation_failure_blocks_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let controller = FakeBrokenController::new();
        let updater = Updater::new(profile, Box::new(FakeProvider), Box::new(controller));

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        let err = updater.execute(&prepared).await.unwrap_err();
        match err {
            PackError::Validation(message) => {
                assert!(
                    message.contains("controller is not usable"),
                    "message: {message}"
                );
                assert!(
                    message.contains("The new version was not committed."),
                    "message: {message}"
                );
                assert!(message.contains("Rollback snapshot:"), "message: {message}");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }

        assert!(!server_root.join(".packctl").join("state.json").exists());
        assert_eq!(
            snapshot_count(&server_root),
            1,
            "snapshot is created before validation runs"
        );
    }

    #[test]
    fn from_profile_builds_provider_and_controller() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = profile(&tmp.path().join("server"), &tmp.path().join("overlay"));

        let updater = Updater::from_profile(&profile).unwrap();

        assert_eq!(updater.profile.name, "test-server");
        assert_eq!(updater.pack_ref().project_id, 42);
        assert!(updater.pack_ref().slug.is_empty());
    }
}
