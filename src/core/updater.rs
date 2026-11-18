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

use crate::config::profile::ServerProfile;
use crate::controllers::ServerController;
use crate::core::adopt::{AdoptContext, AdoptOptions, AdoptReport, classify_server};
use crate::core::detect::{Detection, MAX_CANDIDATES, live_jar_prints, pick, score};
use crate::core::executor::UpdateExecutor;
use crate::core::ignore::IgnoreRules;
use crate::core::loader::{detect_installed_loader, loader_notice};
use crate::core::overlay::{OverlayEngine, OverlayFile};
use crate::core::ownership::FilePolicy;
use crate::core::planner::PlanNotice;
use crate::core::planner::{UpdatePlan, UpdatePlanner};
use crate::core::reserved::{IGNORE_FILE, ReservedPaths};
use crate::core::snapshot::{Snapshot, create_snapshot, create_state_refresh_snapshot};
use crate::core::staging::StagingDir;
use crate::core::state::{InstalledState, ManagedFile, StateStore};
use crate::core::validation::{Severity, has_errors, validate};
use crate::error::{PackError, Result};
use crate::providers::{
    PackProvider, PackRef, PackVersion, PreparedPack, ResolvedPackVersion, VersionSelector,
};

/// Everything needed to run a single planned update.
pub struct PreparedUpdate {
    pub overlay_files: Vec<OverlayFile>,
    pub plan: UpdatePlan,
    pub prepared: PreparedPack,
    /// The `.packctlignore` rules the plan was built with.
    pub ignore: IgnoreRules,
    /// The paths pack content was kept away from.
    pub reserved: ReservedPaths,
    /// The installed state the plan was built against.
    pub state: InstalledState,
    /// Held so the staged sources remain valid for the duration of execution.
    pub staging: StagingDir,
    /// The provider and project the new state records.
    pub source: StateSource,
}

impl PreparedUpdate {
    /// The state a successful execution records (without the timestamp).
    pub fn next_state(&self) -> InstalledState {
        let (managed_files, overlay_shadowed) = managed_files_for(
            &FilePolicy::default_policy(),
            &self.prepared,
            &self.overlay_files,
            &self.ignore,
            &self.reserved,
            &self.state,
        );
        InstalledState {
            installed_version: Some(self.plan.to_version.clone()),
            provider_version_id: Some(self.plan.to_id.clone()),
            managed_files,
            overlay_shadowed,
            overlay_applied: overlay_applied_for(&self.overlay_files),
            provider: Some(self.source.provider.clone()),
            project: self.source.project.clone(),
            minecraft: target_minecraft(&self.prepared),
            last_successful_update: self.state.last_successful_update,
            adopted: self.state.adopted.clone(),
        }
    }

    /// True when executing this update changes anything: files, or only the
    /// recorded state (see [`UpdatePlan::needs_state_commit`]).
    pub fn needs_state_commit(&self) -> bool {
        self.plan
            .needs_state_commit(&self.state, &self.next_state())
    }

    /// The risk of a first update of a server packctl has no record of
    /// (see [`first_update_risk`]).
    pub fn first_update_risk(&self) -> Option<FirstUpdateRisk> {
        first_update_risk(&self.state, &self.plan)
    }

    /// True when executing this update needs `--force-sync`: the plan
    /// deletes files packctl never installed or overwrites live changes it
    /// did not make ([`UpdatePlan::needs_force_sync`]), or it is the first
    /// update of an unmanaged server that would delete or overwrite files
    /// ([`first_update_risk`]).
    pub fn needs_force_sync(&self) -> bool {
        self.plan.needs_force_sync() || self.first_update_risk().is_some()
    }
}

/// What the first update of a server packctl has no record of would destroy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct FirstUpdateRisk {
    /// Files not in the pack that would be deleted.
    pub removals: usize,
    /// Files that differ from the pack (or from the overlay) that would be
    /// overwritten.
    pub overwrites: usize,
}

/// The risk of running `plan` on a server whose `state` is empty: `Some`
/// when the plan deletes or overwrites files packctl did not write, which on
/// such a server means admin changes (added mods, edited configs). The
/// server should be recorded with `packctl adopt` first.
pub fn first_update_risk(state: &InstalledState, plan: &UpdatePlan) -> Option<FirstUpdateRisk> {
    if !state.is_empty() {
        return None;
    }
    let removals = plan.unmanaged_removals.len();
    let overwrites = plan.modifications.len()
        + plan
            .overlay_changes
            .iter()
            .filter(|change| change.live_drift)
            .count();
    (removals + overwrites > 0).then_some(FirstUpdateRisk {
        removals,
        overwrites,
    })
}

/// Everything `packctl adopt` needs: the staged pack version and how the
/// live server compares with it.
pub struct PreparedAdoption {
    pub prepared: PreparedPack,
    /// Held so the staged files stay available until the adoption is done.
    pub staging: StagingDir,
    pub report: AdoptReport,
    /// The overlay as it was scanned.
    pub overlay_files: Vec<OverlayFile>,
    /// The installed state when the adoption was prepared.
    pub state: InstalledState,
}

/// Where a profile's pack comes from, as recorded in the installed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSource {
    /// The provider name (`curseforge`, `modrinth`, `local`).
    pub provider: String,
    /// The project id, or the slug when there is no id; `None` for local
    /// archives.
    pub project: Option<String>,
}

impl StateSource {
    /// The source a profile follows now.
    pub fn for_profile(profile: &ServerProfile) -> Self {
        let pack = crate::providers::pack_ref(profile);
        let project = if pack.project_id.is_empty() {
            pack.slug
        } else {
            pack.project_id
        };
        StateSource {
            provider: profile.pack.provider.as_str().to_string(),
            project: (!project.is_empty()).then_some(project),
        }
    }

    /// True when `state` was recorded from a different provider or project.
    /// State written before these were recorded never differs.
    pub fn differs_from(&self, state: &InstalledState) -> bool {
        let provider_changed = state
            .provider
            .as_ref()
            .is_some_and(|provider| *provider != self.provider);
        let project_changed = state
            .project
            .as_ref()
            .is_some_and(|project| Some(project) != self.project.as_ref());
        provider_changed || project_changed
    }
}

/// The installed state as the planner sees it: version ids recorded from
/// another provider or project mean nothing here, so they are dropped and
/// the plan is built against the recorded files alone.
fn planning_view(state: InstalledState, source: &StateSource) -> InstalledState {
    if source.differs_from(&state) {
        InstalledState {
            provider_version_id: None,
            minecraft: None,
            ..state
        }
    } else {
        state
    }
}

/// The notice shown when the profile now follows another provider or
/// project than the one the installed state came from.
fn provider_changed_notice(state: &InstalledState, now: &StateSource) -> String {
    let describe = |provider: &str, project: Option<&str>| match project {
        Some(project) => format!("{provider} project {project}"),
        None => provider.to_string(),
    };
    let recorded = describe(
        state.provider.as_deref().unwrap_or(&now.provider),
        state.project.as_deref(),
    );
    let current = describe(&now.provider, now.project.as_deref());
    format!(
        "Provider changed\n  Recorded state came from {recorded}; this profile now follows \
         {current}.\n  Files are compared by content; version ids are not comparable."
    )
}

/// The Minecraft version a prepared pack targets: the one the pack declares,
/// else the provider's only listed Minecraft version.
pub(crate) fn target_minecraft(prepared: &PreparedPack) -> Option<String> {
    prepared
        .requirements
        .as_ref()
        .and_then(|requirements| requirements.minecraft.clone())
        .or_else(|| match prepared.version.game_versions.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        })
}

/// The version "latest" means for an installed server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LatestChoice {
    pub id: String,
    /// Set when the newest version overall was passed over because it
    /// targets another Minecraft version.
    pub notice: Option<String>,
}

/// Picks "latest" for a server that has `state` installed, from `versions`
/// sorted newest first.
///
/// Packs often publish parallel lines for several Minecraft versions, and
/// moving a world to another Minecraft version is destructive, so only
/// versions that list the installed Minecraft version (the recorded one, else
/// the Minecraft versions of the installed version) are candidates; a
/// version that lists none is kept, since nothing is known about it.
/// Versions published before the installed one are never picked. Returns
/// `Ok(None)` when nothing restricts the choice (the installed version is
/// not listed and no Minecraft version is recorded), and an error when every
/// version was ruled out.
pub(crate) fn latest_compatible(
    versions: &[PackVersion],
    state: &InstalledState,
) -> Result<Option<LatestChoice>> {
    let installed = state
        .provider_version_id
        .as_deref()
        .and_then(|id| versions.iter().find(|v| v.id == id));
    let minecraft: Vec<String> = match (&state.minecraft, installed) {
        (Some(recorded), _) => vec![recorded.clone()],
        (None, Some(installed)) => installed.game_versions.clone(),
        (None, None) => Vec::new(),
    };
    let installed_date = installed.and_then(|v| v.released.as_deref().and_then(parse_released));
    if minecraft.is_empty() && installed_date.is_none() {
        return Ok(None);
    }

    let targets_minecraft = |version: &PackVersion| {
        minecraft.is_empty()
            || version.game_versions.is_empty()
            || version.game_versions.iter().any(|v| minecraft.contains(v))
    };
    let not_older = |version: &PackVersion| match (
        installed_date,
        version.released.as_deref().and_then(parse_released),
    ) {
        (Some(installed), Some(released)) => released >= installed,
        _ => true,
    };
    let Some(pick) = versions
        .iter()
        .find(|v| targets_minecraft(v) && not_older(v))
    else {
        return Err(PackError::NotFound(format!(
            "no newer version targets Minecraft {}; use --to to pick a version",
            minecraft.join(", ")
        )));
    };
    let notice = versions
        .first()
        .filter(|newest| newest.id != pick.id && !targets_minecraft(newest))
        .map(|newest| {
            format!(
                "Newer version {} targets Minecraft {}; use --to to switch",
                newest.name,
                newest.game_versions.join(", ")
            )
        });
    Ok(Some(LatestChoice {
        id: pick.id.clone(),
        notice,
    }))
}

/// A provider release date, when it is RFC 3339.
fn parse_released(released: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(released.trim())
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

/// Options that change how [`Updater::execute`] treats a risky plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecuteOptions {
    /// Run the plan even when it deletes or overwrites files packctl never
    /// installed or overwrites live changes packctl did not make
    /// ([`UpdatePlan::needs_force_sync`]).
    pub force_sync: bool,
    /// How the user runs `packctl adopt` for this server (for example
    /// `packctl adopt atm10`), for the hint in a first-update refusal.
    /// Defaults to `packctl adopt`.
    pub adopt_command: Option<String>,
}

impl ExecuteOptions {
    /// Options that apply a plan needing `--force-sync`.
    pub fn force_sync() -> Self {
        ExecuteOptions {
            force_sync: true,
            ..ExecuteOptions::default()
        }
    }

    fn adopt_command(&self) -> &str {
        self.adopt_command.as_deref().unwrap_or("packctl adopt")
    }
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

    /// Build an updater from a profile's configured provider (see
    /// [`crate::providers::from_profile`]) and controller.
    pub fn from_profile(profile: &ServerProfile) -> Result<Self> {
        let provider = crate::providers::from_profile(profile)?;
        let controller = crate::controllers::from_profile(&profile.controller)?;
        Ok(Updater {
            profile: profile.clone(),
            provider,
            controller,
        })
    }

    /// The upstream pack this profile follows.
    pub fn pack_ref(&self) -> PackRef {
        crate::providers::pack_ref(&self.profile)
    }

    /// Loads the installed state recorded under the server root.
    pub fn load_state(&self) -> Result<InstalledState> {
        StateStore::at(&self.profile.server.root)?.load()
    }

    /// Resolves the version to prepare.
    ///
    /// `Latest` on a server with a recorded version stays on the recorded
    /// Minecraft version and never goes back in time (see
    /// [`latest_compatible`]); the second value is a notice about a newer
    /// version that was passed over. Any other selector, and a server with
    /// no recorded version, resolves as asked.
    async fn resolve_target(
        &self,
        selector: &VersionSelector,
        state: &InstalledState,
    ) -> Result<(ResolvedPackVersion, Option<String>)> {
        let pack = self.pack_ref();
        let latest = matches!(selector, VersionSelector::Latest);
        if !latest || state.provider_version_id.is_none() {
            let resolved = self.provider.resolve_version(&pack, selector).await?;
            return Ok((resolved, None));
        }
        let versions = self.provider.list_versions(&pack).await?;
        let Some(choice) = latest_compatible(&versions, state)? else {
            let resolved = self.provider.resolve_version(&pack, selector).await?;
            return Ok((resolved, None));
        };
        let selector = if versions.first().is_some_and(|v| v.id == choice.id) {
            VersionSelector::Latest
        } else {
            VersionSelector::Id(choice.id.clone())
        };
        let resolved = self.provider.resolve_version(&pack, &selector).await?;
        Ok((resolved, choice.notice))
    }

    /// Prepares an update without touching the live server.
    ///
    /// Resolves the requested version, stages the upstream pack, scans the
    /// overlay, and builds the exact [`UpdatePlan`] that `execute` will run.
    pub async fn prepare_update(&self, selector: &VersionSelector) -> Result<PreparedUpdate> {
        let state = self.load_state()?;

        // Version ids from another provider or project mean nothing here, so
        // plan against the recorded files without the recorded version id.
        let source = StateSource::for_profile(&self.profile);
        let changed_notice = source
            .differs_from(&state)
            .then(|| provider_changed_notice(&state, &source));
        let state = planning_view(state, &source);

        let (resolved, latest_notice) = self.resolve_target(selector, &state).await?;
        let staging = StagingDir::create_default()?;
        let prepared = self.provider.prepare(&resolved, &staging.root).await?;
        let overlay_files = OverlayEngine::new(self.profile.overlay.path.clone()).scan()?;
        let ignore = load_ignore_rules(&self.profile.server.root)?;
        let reserved = ReservedPaths::for_profile(&self.profile);

        let mut plan = UpdatePlanner::new(FilePolicy::default_policy())
            .with_ignore(ignore.clone())
            .with_reserved(reserved.clone())
            .build_plan_for_server(&state, &prepared, &overlay_files, &self.profile.server.root)?;
        let loader = plan.requirements.as_ref().and_then(|requirements| {
            let detected = detect_installed_loader(&self.profile.server.root);
            loader_notice(requirements, detected.as_ref())
        });
        let duplicates = duplicate_mod_notices(
            &prepared,
            &overlay_files,
            &ignore,
            &reserved,
            &FilePolicy::default_policy(),
            &self.profile.server.root,
        )?;
        plan.notices.extend(
            [changed_notice, latest_notice]
                .into_iter()
                .flatten()
                .chain(duplicates)
                .map(PlanNotice::general)
                .chain(loader),
        );
        Ok(PreparedUpdate {
            overlay_files,
            plan,
            prepared,
            ignore,
            reserved,
            state,
            staging,
            source,
        })
    }

    /// Prepares an adoption without touching the server: stages the given
    /// version and classifies every live file against it (see
    /// [`classify_server`]).
    pub async fn prepare_adoption(
        &self,
        selector: &VersionSelector,
        opts: &AdoptOptions,
    ) -> Result<PreparedAdoption> {
        let state = self.load_state()?;
        let resolved = self
            .provider
            .resolve_version(&self.pack_ref(), selector)
            .await?;
        let staging = StagingDir::create_default()?;
        let prepared = self.provider.prepare(&resolved, &staging.root).await?;
        let overlay_files = OverlayEngine::new(self.profile.overlay.path.clone()).scan()?;
        let (ignore, had_block) = load_ignore_rules_for_adoption(&self.profile.server.root)?;
        let reserved = ReservedPaths::for_profile(&self.profile);
        let policy = FilePolicy::default_policy();
        let mut report = classify_server(
            &AdoptContext {
                profile_name: &self.profile.name,
                prepared: &prepared,
                overlay: &overlay_files,
                ignore: &ignore,
                reserved: &reserved,
                policy: &policy,
                server_root: &self.profile.server.root,
                overlay_root: &self.profile.overlay.path,
            },
            opts,
        )?;
        if had_block {
            report.warnings.push(format!(
                "{IGNORE_FILE} already has a 'packctl adopt' block; the paths it covered were \
                 classified again above and the block is replaced"
            ));
        }
        Ok(PreparedAdoption {
            prepared,
            staging,
            report,
            overlay_files,
            state,
        })
    }

    /// Guesses which pack version the server runs from the jars in its
    /// `mods/` folder, among the newest [`MAX_CANDIDATES`] versions (see
    /// [`crate::core::detect`]).
    pub async fn detect_version(&self) -> Result<Detection> {
        let pack = self.pack_ref();
        let mut versions = self.provider.list_versions(&pack).await?;
        versions.truncate(MAX_CANDIDATES);
        if versions.is_empty() {
            return Err(PackError::NotFound(format!(
                "no versions available for server '{}'",
                self.profile.name
            )));
        }
        let fingerprints = self.provider.version_fingerprints(&pack, &versions).await?;
        let live = live_jar_prints(&self.profile.server.root, &fingerprints)?;
        if live.is_empty() {
            return Err(PackError::Config(format!(
                "no jars found in '{}'; pass --as <version>",
                self.profile.server.root.join("mods").display()
            )));
        }
        let scores = fingerprints
            .iter()
            .map(|candidate| (candidate.version.clone(), score(&live, candidate)))
            .collect();
        Ok(pick(scores))
    }

    /// Executes a prepared update against the live server.
    ///
    /// Follows design notes "Fundamental Update Model" exactly: an update
    /// that changes nothing is a no-op; a plan that would delete files packctl
    /// never installed or overwrite live changes it did not make is refused
    /// unless `opts.force_sync` is set; a plan without file work only records
    /// the new state; otherwise the server is stopped, a rollback snapshot is
    /// created, upstream changes and then the overlay are applied, the result
    /// is validated, the server is started, and only after all of that
    /// succeeds is the new state committed. Every mutation-phase failure after
    /// the snapshot exists carries the snapshot location so the previous
    /// version can be restored.
    pub async fn execute(
        &self,
        prepared: &PreparedUpdate,
        opts: &ExecuteOptions,
    ) -> Result<UpdateOutcome> {
        let plan = &prepared.plan;
        let server_root = &self.profile.server.root;

        if !prepared.needs_state_commit() {
            return Ok(UpdateOutcome {
                upstream_writes: 0,
                overlay_copied: 0,
                snapshot: None,
                committed: false,
            });
        }

        if prepared.needs_force_sync() && !opts.force_sync {
            return Err(PackError::Config(force_sync_message(
                &self.profile.name,
                plan,
                first_update_risk(&prepared.state, plan),
                opts.adopt_command(),
            )));
        }

        // Hold the lock through preflight and commit so no concurrent rollback
        // or update can act on the same server state.
        let _lock = StateStore::at(server_root)?.lock()?;
        self.check_unchanged_since_prepare(prepared)?;

        if plan.is_empty() {
            return self.commit_state_only(prepared);
        }

        let issues = validate(
            &self.profile,
            Some(&prepared.prepared),
            &prepared.overlay_files,
            self.controller.as_ref(),
        )
        .await?;
        if has_errors(&issues) {
            return Err(validation_error(&issues));
        }

        self.controller.stop().await?;

        // The plan was built while the server ran. Mods save data on
        // shutdown, so look again now that it stopped: a file that changed
        // meanwhile must not be destroyed without --force-sync.
        if !opts.force_sync
            && let Err(error) = self.recheck_after_stop(prepared, opts)
        {
            return Err(self.start_after_refusal(error).await);
        }

        let snapshot = snapshot_before_mutation(plan, server_root)?;

        let executor = UpdateExecutor::new(server_root.clone());
        let upstream_writes = executor
            .apply_plan(plan, &prepared.prepared.root)
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
        .await
        .map_err(|error| snapshot_context(&snapshot, error))?;
        if has_errors(&issues) {
            return Err(snapshot_context(&snapshot, validation_error(&issues)));
        }

        self.controller
            .start()
            .await
            .map_err(|error| snapshot_context(&snapshot, error))?;

        let new_state = InstalledState {
            last_successful_update: Some(Utc::now()),
            ..prepared.next_state()
        };
        StateStore::at(server_root)
            .and_then(|store| store.save(&new_state))
            .map_err(|error| snapshot_context(&snapshot, error))?;

        Ok(UpdateOutcome {
            upstream_writes,
            overlay_copied,
            snapshot: Some(snapshot),
            committed: true,
        })
    }

    /// Refuses to run a plan when `state.json` or `.packctlignore` changed
    /// since it was prepared (another packctl command ran in between, say
    /// an adopt or a rollback): the plan and the state it would save were
    /// derived from the old ones. The caller holds the server lock.
    fn check_unchanged_since_prepare(&self, prepared: &PreparedUpdate) -> Result<()> {
        let state = planning_view(self.load_state()?, &prepared.source);
        let ignore = load_ignore_rules(&self.profile.server.root)?;
        if state != prepared.state || ignore != prepared.ignore {
            return Err(PackError::Config(format!(
                "the packctl state of '{}' changed since the plan was prepared (another packctl \
                 command ran meanwhile); nothing was changed. Run 'packctl plan' again.",
                self.profile.name
            )));
        }
        Ok(())
    }

    /// Plans again against the stopped server and refuses when the plan now
    /// deletes or overwrites something it may only touch with
    /// `--force-sync` (see [`PreparedUpdate::needs_force_sync`]).
    fn recheck_after_stop(&self, prepared: &PreparedUpdate, opts: &ExecuteOptions) -> Result<()> {
        let plan = UpdatePlanner::new(FilePolicy::default_policy())
            .with_ignore(prepared.ignore.clone())
            .with_reserved(prepared.reserved.clone())
            .build_plan_for_server(
                &prepared.state,
                &prepared.prepared,
                &prepared.overlay_files,
                &self.profile.server.root,
            )?;
        let risk = first_update_risk(&prepared.state, &plan);
        if plan.needs_force_sync() || risk.is_some() {
            return Err(PackError::Config(format!(
                "files changed while the server stopped, so the update was not applied.\n{}",
                force_sync_message(&self.profile.name, &plan, risk, opts.adopt_command())
            )));
        }
        Ok(())
    }

    /// Starts the server again after an update was refused once it had
    /// stopped, and returns `refusal` (noting a failed start).
    async fn start_after_refusal(&self, refusal: PackError) -> PackError {
        match self.controller.start().await {
            Ok(()) => refusal,
            Err(error) => PackError::Other(format!(
                "{refusal}\nStarting the server again failed too: {error}"
            )),
        }
    }

    /// Records the new state for a plan without file work.
    ///
    /// The server already matches the target version (for example a
    /// hand-installed server that is identical to the pack, or a version bump
    /// that only touches overlay-owned files), so the server is neither
    /// stopped nor validated. Only `state.json` is snapshotted.
    ///
    /// A commit that records a different version is a normal rollback point:
    /// `packctl rollback` then reverts just the record and leaves the files
    /// alone. A commit that keeps the recorded version (such as the one-time
    /// re-record of a state file written before `overlay_applied` and
    /// `overlay_shadowed` existed) is marked as a state refresh, which
    /// rollback passes over so it never hides the last real change.
    /// The caller holds the server lock.
    fn commit_state_only(&self, prepared: &PreparedUpdate) -> Result<UpdateOutcome> {
        let server_root = &self.profile.server.root;
        let state_path = server_root.join(STATE_REL_PATH);
        let files: Vec<&Path> = if state_path.exists() {
            vec![state_path.as_path()]
        } else {
            Vec::new()
        };
        let plan = &prepared.plan;
        let snapshot = if plan.from_id.as_deref() == Some(plan.to_id.as_str()) {
            create_state_refresh_snapshot(server_root, &files, &[STATE_REL_PATH])?
        } else {
            create_snapshot(server_root, &files, &[STATE_REL_PATH])?
        };

        let new_state = InstalledState {
            last_successful_update: Some(Utc::now()),
            ..prepared.next_state()
        };
        StateStore::at(server_root)
            .and_then(|store| store.save(&new_state))
            .map_err(|error| snapshot_context(&snapshot, error))?;

        Ok(UpdateOutcome {
            upstream_writes: 0,
            overlay_copied: 0,
            snapshot: Some(snapshot),
            committed: true,
        })
    }
}

/// Relative path of the state file inside the server root.
const STATE_REL_PATH: &str = ".packctl/state.json";

/// Loads the `.packctlignore` rules from the server root, if present.
///
/// A missing file means no ignore rules; an unparsable file is a hard error so
/// a typo can never silently widen the sweep.
pub(crate) fn load_ignore_rules(server_root: &Path) -> Result<IgnoreRules> {
    match read_ignore_file(server_root)? {
        Some(content) => crate::core::ignore::parse_rules(&content),
        None => Ok(IgnoreRules::default()),
    }
}

/// The `.packctlignore` rules `packctl adopt` classifies against: the whole
/// file except the managed `packctl adopt` block, which the adoption
/// replaces. The flag is true when such a block exists.
///
/// Classifying against the old block would skip every path it covers, so
/// none of them would get an entry or a rule, and replacing the block would
/// silently drop their protection (an ignored edited config would be
/// overwritten on the next update, a removed mod re-added).
pub(crate) fn load_ignore_rules_for_adoption(server_root: &Path) -> Result<(IgnoreRules, bool)> {
    let Some(content) = read_ignore_file(server_root)? else {
        return Ok((IgnoreRules::default(), false));
    };
    let outside = crate::core::ignore::without_managed_block(&content);
    let had_block = outside.len() != content.len();
    Ok((crate::core::ignore::parse_rules(&outside)?, had_block))
}

/// The contents of `.packctlignore`, or `None` when there is none.
fn read_ignore_file(server_root: &Path) -> Result<Option<String>> {
    let path = server_root.join(IGNORE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PackError::io(format!("read '{}'", path.display()), error)),
    }
}

/// Most paths listed by name in the force-sync refusal.
const MAX_REFUSAL_LISTED: usize = 10;

/// The refusal for a plan that needs `--force-sync` but did not get it.
///
/// On a server packctl has no record of (`risk` is set), the message also
/// points at `adopt_command` (how the user runs `packctl adopt` for this
/// server), which records the server without touching it.
fn force_sync_message(
    profile_name: &str,
    plan: &UpdatePlan,
    risk: Option<FirstUpdateRisk>,
    adopt_command: &str,
) -> String {
    if let Some(risk) = risk {
        let mut message = format!(
            "refusing the first update of '{profile_name}': packctl has no record of this \
             server, and the plan would delete {} file(s) and overwrite {} file(s) that differ \
             from the pack.",
            risk.removals, risk.overwrites
        );
        let overwritten: Vec<&Path> = plan
            .modifications
            .iter()
            .map(|change| change.rel_path.as_path())
            .chain(plan.live_drift_paths())
            .collect();
        if !plan.unmanaged_removals.is_empty() {
            message.push_str("\nWould delete:");
            list_paths(
                &mut message,
                plan.unmanaged_removals.iter().map(PathBuf::as_path),
            );
        }
        if !overwritten.is_empty() {
            message.push_str("\nWould overwrite:");
            list_paths(&mut message, overwritten.into_iter());
        }
        message.push_str(&format!(
            "\nIf the server already runs this pack, record it first:\n  \
             {adopt_command} --as <version>\n\
             To converge onto the pack anyway (deleting and overwriting those files), \
             re-run with --force-sync."
        ));
        return message;
    }
    let mut message = format!("refusing to update '{profile_name}':");
    let removals = &plan.unmanaged_removals;
    if !removals.is_empty() {
        message.push_str(&format!(
            "\nthe plan would delete {} file(s) packctl never installed:",
            removals.len()
        ));
        list_paths(&mut message, removals.iter().map(PathBuf::as_path));
    }
    let overwrites = &plan.unmanaged_overwrites;
    if !overwrites.is_empty() {
        message.push_str(&format!(
            "\nthe pack would overwrite {} file(s) packctl never installed:",
            overwrites.len()
        ));
        list_paths(&mut message, overwrites.iter().map(PathBuf::as_path));
    }
    let drift = plan.live_drift_paths();
    if !drift.is_empty() {
        message.push_str(&format!(
            "\nthe overlay would overwrite {} live file(s) that changed outside packctl:",
            drift.len()
        ));
        list_paths(&mut message, drift.into_iter());
    }
    message.push_str(
        "\nKeep what you need first: copy files into the overlay or add .packctlignore rules.\n\
         To converge onto the pack anyway, re-run with --force-sync.",
    );
    message
}

fn list_paths<'a>(message: &mut String, paths: impl ExactSizeIterator<Item = &'a Path>) {
    let total = paths.len();
    for path in paths.take(MAX_REFUSAL_LISTED) {
        message.push_str(&format!("\n  {}", path.display()));
    }
    if total > MAX_REFUSAL_LISTED {
        message.push_str(&format!("\n  … and {} more", total - MAX_REFUSAL_LISTED));
    }
}

/// Where a jar in the final `mods/` set comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum JarOrigin {
    Upstream,
    Overlay,
    Kept,
}

impl JarOrigin {
    fn label(self) -> &'static str {
        match self {
            JarOrigin::Upstream => "upstream",
            JarOrigin::Overlay => "overlay",
            JarOrigin::Kept => "kept",
        }
    }
}

/// True for `mods/<name>.jar` directly inside `mods/`.
fn is_top_level_mod_jar(rel: &Path) -> bool {
    let mut components = rel.components();
    matches!(
        (components.next(), components.next(), components.next()),
        (Some(first), Some(_), None) if first.as_os_str() == "mods"
    ) && crate::fs::paths::is_jar_path(rel)
}

/// Notices for mod ids that more than one jar of the final `mods/` folder
/// declares after the update: upstream jars (from staging) that are not
/// ignored and not overlay-owned, the overlay's jars, and live jars the
/// update keeps (ignored or persistent). Only jar metadata counts here; a
/// file name is too weak a hint to warn about.
fn duplicate_mod_notices(
    prepared: &PreparedPack,
    overlay: &[OverlayFile],
    ignore: &IgnoreRules,
    reserved: &ReservedPaths,
    policy: &FilePolicy,
    server_root: &Path,
) -> Result<Vec<String>> {
    let overlay_keys: HashSet<String> =
        overlay.iter().map(|file| rel_key(&file.rel_path)).collect();
    let mut jars: Vec<(String, PathBuf, JarOrigin)> = Vec::new();
    let mut desired = HashSet::new();
    for file in &prepared.files {
        let rel = file.rel_path.as_path();
        let key = rel_key(rel);
        desired.insert(key.clone());
        if !is_top_level_mod_jar(rel)
            || overlay_keys.contains(&key)
            || ignore.is_ignored(rel)
            || policy.is_persistent(rel)
            || reserved.is_reserved(rel)
        {
            continue;
        }
        jars.push((key, prepared.root.join(rel), JarOrigin::Upstream));
    }
    for file in overlay {
        if is_top_level_mod_jar(&file.rel_path) {
            jars.push((
                rel_key(&file.rel_path),
                file.source.clone(),
                JarOrigin::Overlay,
            ));
        }
    }
    let mods = server_root.join("mods");
    if let Ok(entries) = std::fs::read_dir(&mods) {
        for entry in entries.flatten() {
            let rel = Path::new("mods").join(entry.file_name());
            let key = rel_key(&rel);
            let kept = ignore.is_ignored(&rel) || policy.is_persistent(&rel);
            let is_file = std::fs::symlink_metadata(entry.path()).is_ok_and(|m| m.is_file());
            if is_file
                && kept
                && is_top_level_mod_jar(&rel)
                && !desired.contains(&key)
                && !overlay_keys.contains(&key)
            {
                jars.push((key, entry.path(), JarOrigin::Kept));
            }
        }
    }

    let mut by_id: std::collections::BTreeMap<String, Vec<(String, JarOrigin)>> =
        std::collections::BTreeMap::new();
    for (key, path, origin) in jars {
        // An unreadable jar is reported elsewhere (validation); it simply
        // contributes no ids here.
        let Ok(ids) = crate::core::modid::jar_mod_ids(&path) else {
            continue;
        };
        for id in ids {
            by_id.entry(id).or_default().push((key.clone(), origin));
        }
    }
    Ok(by_id
        .into_iter()
        .filter(|(_, holders)| holders.len() > 1)
        .map(|(id, mut holders)| {
            holders.sort();
            let mut message = format!("Duplicate mod id '{id}'");
            for (path, origin) in holders {
                message.push_str(&format!("\n  {path} ({})", origin.label()));
            }
            message.push_str(
                "\n  The server will likely refuse to start; remove one copy from the overlay \
                 or .packctlignore.",
            );
            message
        })
        .collect())
}

/// Creates the rollback snapshot covering every path the plan touches.
///
/// Only paths that already exist on disk are copied into the snapshot; planned
/// additions that do not exist yet are still recorded as tracked so a rollback
/// can remove them again. Every overlay target is tracked, not only the ones
/// that were pending at plan time: the overlay is applied after the server
/// stopped, and a mod may have rewritten an in-sync file on shutdown. The
/// pre-update `state.json` is included so a rollback can also restore the
/// previous state metadata (design notes "Rollback", step "Restore state
/// metadata").
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

/// Builds the file records to commit for a prepared upstream pack, returning
/// `(managed, shadowed)`.
///
/// Persistent runtime data, ignored paths and reserved paths are never
/// recorded: packctl does not write them. Pack paths the overlay provides go
/// to `shadowed` with their upstream fingerprint instead of `managed`, because
/// the overlay owns the file on disk but later updates still need to know
/// whether upstream changed it (see design notes "Managed File" and "Overlay
/// Wins").
///
/// An accepted local variant (`local_sha256`, recorded by adopt) is carried
/// over from `previous` while upstream still ships the same content; when
/// upstream changes the file, the update writes the new content and the
/// alias is dropped.
pub(crate) fn managed_files_for(
    policy: &FilePolicy,
    prepared: &PreparedPack,
    overlay: &[OverlayFile],
    ignore: &IgnoreRules,
    reserved: &ReservedPaths,
    previous: &InstalledState,
) -> (HashMap<String, ManagedFile>, HashMap<String, ManagedFile>) {
    let overlay_paths: HashSet<String> =
        overlay.iter().map(|file| rel_key(&file.rel_path)).collect();

    let mut managed = HashMap::new();
    let mut shadowed = HashMap::new();
    for file in &prepared.files {
        let rel = file.rel_path.as_path();
        if policy.is_persistent(rel) || ignore.is_ignored(rel) || reserved.is_reserved(rel) {
            continue;
        }
        let key = rel_key(rel);
        let mut record = ManagedFile::new(file.sha256.clone(), file.size);
        if let Some(local) = previous
            .managed_files
            .get(&key)
            .filter(|previous| previous.sha256 == file.sha256)
            .and_then(|previous| previous.local_sha256.clone())
        {
            record.local_sha256 = Some(local);
        }
        if overlay_paths.contains(&key) {
            shadowed.insert(key, record);
        } else {
            managed.insert(key, record);
        }
    }
    (managed, shadowed)
}

/// The fingerprints of the overlay files as they are written to the server.
fn overlay_applied_for(overlay: &[OverlayFile]) -> HashMap<String, ManagedFile> {
    overlay
        .iter()
        .map(|file| {
            (
                rel_key(&file.rel_path),
                ManagedFile::new(file.sha256.clone(), file.size),
            )
        })
        .collect()
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
    let message = format!(
        "{cause}\nThe new version was not committed.\nRollback snapshot:\n  {}",
        snapshot.dir.display()
    );
    match cause {
        PackError::Validation(_) => PackError::Validation(message),
        _ => PackError::Other(message),
    }
}

fn validation_error(issues: &[crate::core::validation::ValidationIssue]) -> PackError {
    let messages: Vec<String> = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .map(|issue| issue.message.clone())
        .collect();
    PackError::Validation(messages.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profile::{
        CommandConfig, ControllerKind, ControllerSection, OverlaySection, PackSection,
        ServerSection,
    };
    use crate::controllers::ServerStatus;
    use crate::fs::hashing::sha256_bytes;
    use crate::providers::{PackVersion, PreparedFile, ResolvedPackVersion};
    use crate::testutil::{FakeController, FakeProvider};

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
                requirements: None,
                notices: Vec::new(),
            })
        }
    }

    fn profile(server_root: &Path, overlay: &Path) -> ServerProfile {
        ServerProfile {
            name: "test-server".to_string(),
            server: ServerSection {
                root: server_root.to_path_buf(),
            },
            pack: PackSection::curseforge(42),
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
            Box::new(FakeProvider::single()),
            Box::new(controller.clone()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();

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
        let updater = Updater::new(
            profile,
            Box::new(FakeProvider::single()),
            Box::new(controller),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        let err = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err, PackError::Validation(_)),
            "expected preflight Validation error, got {err:?}"
        );

        assert!(server_root.join(".packctl").join("update.lock").exists());
        assert!(!server_root.join(".packctl").join("state.json").exists());
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
                ManagedFile::new(sha256_bytes(b"old-content"), 11),
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
        let updater = Updater::new(
            profile,
            Box::new(FakeProvider::single()),
            Box::new(controller),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(!prepared.plan.is_empty());
        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();

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
    async fn first_update_sweeps_stale_pack_folder_files() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("mods")).unwrap();
        std::fs::write(server_root.join("mods/stale-old.jar"), b"old").unwrap();
        std::fs::create_dir_all(server_root.join("world/region")).unwrap();
        std::fs::write(server_root.join("world/region/r.mca"), b"world").unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let controller = FakeController::new();
        let updater = Updater::new(
            profile,
            Box::new(FakeProvider::single()),
            Box::new(controller),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(
            prepared
                .plan
                .removals
                .iter()
                .any(|c| c.rel_path == Path::new("mods/stale-old.jar")),
            "a stale file in a pack-owned folder must be planned for removal even on a first update"
        );

        let outcome = updater
            .execute(&prepared, &ExecuteOptions::force_sync())
            .await
            .unwrap();
        assert!(outcome.committed);
        assert!(
            !server_root.join("mods/stale-old.jar").exists(),
            "stale file must be removed"
        );
        assert!(server_root.join("mods/upstream.jar").exists());
        assert!(server_root.join("config/upstream.toml").exists());
        assert_eq!(
            std::fs::read(server_root.join("world/region/r.mca")).unwrap(),
            b"world"
        );
    }

    #[tokio::test]
    async fn prepare_update_respects_packctlignore() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("mods")).unwrap();
        std::fs::write(server_root.join("mods/stale-old.jar"), b"old").unwrap();
        std::fs::write(server_root.join("mods/custom-kept.jar"), b"keep").unwrap();
        std::fs::write(
            server_root.join(".packctlignore"),
            b"mods/custom-kept.jar\n",
        )
        .unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let updater = Updater::new(
            profile,
            Box::new(FakeProvider::single()),
            Box::new(FakeController::new()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();

        assert!(
            prepared
                .plan
                .removals
                .iter()
                .any(|c| c.rel_path == Path::new("mods/stale-old.jar")),
            "unignored stale file must be planned for removal"
        );
        assert!(
            !prepared
                .plan
                .removals
                .iter()
                .any(|c| c.rel_path == Path::new("mods/custom-kept.jar")),
            "ignored file must not be planned for removal"
        );
    }

    #[tokio::test]
    async fn first_update_refuses_unmanaged_removals_without_force_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("mods")).unwrap();
        std::fs::write(server_root.join("mods/admin-added.jar"), b"admin").unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let controller = FakeController::new();
        let updater = Updater::new(
            profile,
            Box::new(FakeProvider::single()),
            Box::new(controller.clone()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert_eq!(
            prepared.plan.unmanaged_removals,
            vec![PathBuf::from("mods/admin-added.jar")]
        );

        let err = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap_err();
        let PackError::Config(message) = &err else {
            panic!("expected a Config error, got {err:?}");
        };
        assert!(message.contains("mods/admin-added.jar"), "{message}");
        assert!(message.contains("--force-sync"), "{message}");

        assert!(controller.log().is_empty(), "the server was not touched");
        assert!(server_root.join("mods/admin-added.jar").exists());
        assert!(!server_root.join("mods/upstream.jar").exists());
        assert!(!server_root.join(".packctl").join("state.json").exists());
        assert_eq!(snapshot_count(&server_root), 0);
    }

    /// Writes the state the v2 fixture update would record, with `overlay`
    /// already applied.
    fn record_v2(server_root: &Path, overlay: &[(&str, &[u8])]) {
        let mut state = InstalledState {
            installed_version: Some("2.0".to_string()),
            provider_version_id: Some("v2".to_string()),
            last_successful_update: Some(Utc::now()),
            ..InstalledState::default()
        };
        for (rel, data) in overlay {
            state
                .overlay_applied
                .insert(rel.to_string(), fingerprint(data));
        }
        write_state(server_root, &state);
    }

    fn fingerprint(data: &[u8]) -> ManagedFile {
        ManagedFile::new(sha256_bytes(data), data.len() as u64)
    }

    #[tokio::test]
    async fn empty_plan_skips_execution_without_contacting_server() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();
        record_v2(&server_root, &[]);
        let before = std::fs::read(server_root.join(".packctl/state.json")).unwrap();

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
        assert!(!prepared.needs_state_commit());

        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(!outcome.committed);
        assert_eq!(outcome.upstream_writes, 0);
        assert_eq!(outcome.overlay_copied, 0);
        assert!(outcome.snapshot.is_none());
        assert!(controller.log().is_empty());
        assert_eq!(snapshot_count(&server_root), 0);
        assert_eq!(
            std::fs::read(server_root.join(".packctl/state.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn same_version_update_with_overlay_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("config")).unwrap();
        std::fs::create_dir_all(server_root.join("mods")).unwrap();
        // The overlay is already in place on the server.
        std::fs::write(server_root.join("config/motd.conf"), b"hello").unwrap();
        std::fs::write(server_root.join("mods/extra.jar"), b"extra").unwrap();
        record_v2(
            &server_root,
            &[("config/motd.conf", b"hello"), ("mods/extra.jar", b"extra")],
        );
        let before = std::fs::read(server_root.join(".packctl/state.json")).unwrap();

        let overlay = tmp.path().join("overlay");
        std::fs::create_dir_all(overlay.join("config")).unwrap();
        std::fs::create_dir_all(overlay.join("mods")).unwrap();
        std::fs::write(overlay.join("config/motd.conf"), b"hello").unwrap();
        std::fs::write(overlay.join("mods/extra.jar"), b"extra").unwrap();

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
        assert_eq!(prepared.plan.overlay_changes.len(), 2);
        assert!(prepared.plan.is_empty(), "{:?}", prepared.plan);
        assert!(!prepared.needs_state_commit());

        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(!outcome.committed);
        assert!(outcome.snapshot.is_none());
        assert!(controller.log().is_empty(), "the server was not restarted");
        assert_eq!(
            std::fs::read(server_root.join(".packctl/state.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn first_update_skips_identical_live_files() {
        // A hand-installed server that already matches the pack.
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("mods")).unwrap();
        std::fs::create_dir_all(server_root.join("config")).unwrap();
        std::fs::write(server_root.join("mods/upstream.jar"), b"UP").unwrap();
        std::fs::write(server_root.join("config/upstream.toml"), b"TOML").unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let controller = FakeController::new();
        let updater = Updater::new(
            profile,
            Box::new(FakeProvider::single()),
            Box::new(controller.clone()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(prepared.plan.is_empty(), "{:?}", prepared.plan);
        assert!(prepared.needs_state_commit());

        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(outcome.committed, "the version is recorded");
        assert_eq!(outcome.upstream_writes, 0);
        assert!(outcome.snapshot.is_some());
        assert!(controller.log().is_empty(), "the server was not restarted");

        let state = load_state(&server_root);
        assert_eq!(state.installed_version.as_deref(), Some("2.0"));
        assert_eq!(state.provider_version_id.as_deref(), Some("v2"));
        assert_eq!(state.managed_files.len(), 2);
        assert!(state.last_successful_update.is_some());

        // Recording a version is a normal rollback point (not a state
        // refresh): rollback picks it, and restoring it brings back the empty
        // record without touching any server file.
        let snapshot = outcome.snapshot.unwrap();
        assert!(!snapshot.manifest.state_refresh);
        assert_eq!(snapshot.manifest.tracked_paths, vec![STATE_REL_PATH]);
        let snapshots = crate::core::snapshot::list_snapshots(&server_root).unwrap();
        let (target, skipped) = crate::core::snapshot::rollback_target(&snapshots).unwrap();
        assert_eq!((target.dir.as_path(), skipped), (snapshot.dir.as_path(), 0));
        crate::core::snapshot::restore_snapshot(&server_root, &snapshot).unwrap();
        assert!(updater.load_state().unwrap().is_empty());

        // Planning again after a fresh commit is a no-op.
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(!prepared.needs_state_commit());
    }

    #[tokio::test]
    async fn recorded_unmanaged_server_updates_to_v2_without_force_sync() {
        // A hand-installed server identical to v1.
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        for (rel, data) in [
            ("mods/a.jar", b"a-v1".as_slice()),
            ("mods/old.jar", b"old".as_slice()),
            ("config/b.toml", b"b".as_slice()),
        ] {
            let path = server_root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, data).unwrap();
        }

        let overlay = tmp.path().join("overlay");
        let controller = FakeController::new();
        let updater = Updater::new(
            profile(&server_root, &overlay),
            Box::new(FakeProvider::two_versions()),
            Box::new(controller.clone()),
        );

        // Record v1: nothing to write, state only.
        let v1 = updater
            .prepare_update(&VersionSelector::Query("1.0".to_string()))
            .await
            .unwrap();
        assert!(v1.plan.is_empty());
        let outcome = updater
            .execute(&v1, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(outcome.committed);

        // v2 removes and replaces files packctl now knows it installed.
        let v2 = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(v2.plan.unmanaged_removals.is_empty());
        assert!(!v2.plan.needs_force_sync());
        let outcome = updater
            .execute(&v2, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(outcome.committed);
        assert_eq!(
            std::fs::read(server_root.join("mods/a.jar")).unwrap(),
            b"a-v2"
        );
        assert!(!server_root.join("mods/old.jar").exists());
        assert!(server_root.join("mods/c.jar").exists());
        assert_eq!(controller.log(), &["stop", "start"]);
        assert_eq!(
            load_state(&server_root).provider_version_id.as_deref(),
            Some("id-2")
        );
        let update_snapshot = outcome.snapshot.unwrap();

        // A record that is out of date (as after upgrading from a packctl that
        // wrote fewer state fields) triggers a state refresh that keeps v2.
        let mut stale = load_state(&server_root);
        stale.managed_files.clear();
        StateStore::at(&server_root).unwrap().save(&stale).unwrap();
        let again = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(again.plan.is_empty());
        assert!(again.needs_state_commit());
        let record = updater
            .execute(&again, &ExecuteOptions::default())
            .await
            .unwrap()
            .snapshot
            .unwrap();
        assert!(record.manifest.state_refresh);

        // Rollback passes over the refresh and reaches the v1 -> v2 snapshot.
        let snapshots = crate::core::snapshot::list_snapshots(&server_root).unwrap();
        let (target, skipped) = crate::core::snapshot::rollback_target(&snapshots).unwrap();
        assert_eq!(
            (target.dir.as_path(), skipped),
            (update_snapshot.dir.as_path(), 1)
        );
        crate::core::snapshot::restore_snapshot(&server_root, target).unwrap();
        assert_eq!(
            std::fs::read(server_root.join("mods/a.jar")).unwrap(),
            b"a-v1"
        );
        assert!(server_root.join("mods/old.jar").exists());
        assert!(!server_root.join("mods/c.jar").exists());
        assert_eq!(
            load_state(&server_root).provider_version_id.as_deref(),
            Some("id-1")
        );
    }

    #[tokio::test]
    async fn rollback_after_version_record_reverts_only_the_record() {
        // v3 ships exactly the files of v2, so updating v2 -> v3 records the
        // new version without touching the server.
        let mut provider = FakeProvider::two_versions();
        let v2_files = provider.versions[0].2.clone();
        provider
            .versions
            .insert(0, ("id-3".to_string(), "3.0".to_string(), v2_files));

        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();
        let overlay = tmp.path().join("overlay");
        let controller = FakeController::new();
        let updater = Updater::new(
            profile(&server_root, &overlay),
            Box::new(provider),
            Box::new(controller.clone()),
        );

        for version in ["1.0", "2.0"] {
            let prepared = updater
                .prepare_update(&VersionSelector::Query(version.to_string()))
                .await
                .unwrap();
            assert!(!prepared.plan.is_empty());
            updater
                .execute(&prepared, &ExecuteOptions::default())
                .await
                .unwrap();
        }
        assert_eq!(
            std::fs::read(server_root.join("mods/a.jar")).unwrap(),
            b"a-v2"
        );

        let v3 = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(v3.plan.is_empty());
        let record = updater
            .execute(&v3, &ExecuteOptions::default())
            .await
            .unwrap()
            .snapshot
            .unwrap();
        assert!(!record.manifest.state_refresh);
        assert_eq!(
            load_state(&server_root).provider_version_id.as_deref(),
            Some("id-3")
        );

        // Rollback undoes the last change, the v3 record, and keeps the v2
        // files instead of reaching back to the v1 -> v2 file snapshot.
        let snapshots = crate::core::snapshot::list_snapshots(&server_root).unwrap();
        let (target, skipped) = crate::core::snapshot::rollback_target(&snapshots).unwrap();
        assert_eq!((target.dir.as_path(), skipped), (record.dir.as_path(), 0));
        crate::core::snapshot::restore_snapshot(&server_root, target).unwrap();
        let state = load_state(&server_root);
        assert_eq!(state.provider_version_id.as_deref(), Some("id-2"));
        assert_eq!(
            std::fs::read(server_root.join("mods/a.jar")).unwrap(),
            b"a-v2"
        );
        assert!(server_root.join("mods/c.jar").exists());
        assert!(!server_root.join("mods/old.jar").exists());
    }

    #[tokio::test]
    async fn live_drift_on_overlay_target_is_refused_without_force_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("config")).unwrap();
        // packctl applied "adopted"; the server has since rewritten the file.
        std::fs::write(server_root.join("config/quests.snbt"), b"edited in game").unwrap();
        record_v2(&server_root, &[("config/quests.snbt", b"adopted")]);

        let overlay = tmp.path().join("overlay");
        std::fs::create_dir_all(overlay.join("config")).unwrap();
        std::fs::write(overlay.join("config/quests.snbt"), b"adopted").unwrap();

        let controller = FakeController::new();
        let updater = Updater::new(
            profile(&server_root, &overlay),
            Box::new(EmptyFakeProvider),
            Box::new(controller.clone()),
        );
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert_eq!(
            prepared.plan.live_drift_paths(),
            vec![Path::new("config/quests.snbt")]
        );

        let err = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("config/quests.snbt"), "{err}");
        assert_eq!(
            std::fs::read(server_root.join("config/quests.snbt")).unwrap(),
            b"edited in game"
        );
        assert!(controller.log().is_empty());

        let outcome = updater
            .execute(&prepared, &ExecuteOptions::force_sync())
            .await
            .unwrap();
        assert!(outcome.committed);
        assert_eq!(
            std::fs::read(server_root.join("config/quests.snbt")).unwrap(),
            b"adopted"
        );
    }

    #[tokio::test]
    async fn snapshot_tracks_in_sync_overlay_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("config")).unwrap();
        std::fs::write(server_root.join("config/motd.conf"), b"hello").unwrap();
        record_v2(&server_root, &[("config/motd.conf", b"hello")]);

        let overlay = tmp.path().join("overlay");
        std::fs::create_dir_all(overlay.join("config")).unwrap();
        std::fs::write(overlay.join("config/motd.conf"), b"hello").unwrap();

        let updater = Updater::new(
            profile(&server_root, &overlay),
            Box::new(FakeProvider::single()),
            Box::new(FakeController::new()),
        );
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(!prepared.plan.overlay_changes[0].pending);

        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();
        let snapshot = outcome.snapshot.unwrap();
        assert!(
            snapshot
                .manifest
                .tracked_paths
                .iter()
                .any(|p| p == "config/motd.conf"),
            "{:?}",
            snapshot.manifest.tracked_paths
        );
        assert!(snapshot.files_root().join("config/motd.conf").is_file());
    }

    #[test]
    fn managed_files_exclude_ignored_paths_and_shadow_overlay_paths() {
        let prepared = PreparedPack {
            name: "fixture".to_string(),
            version: FakeProvider::version(),
            root: PathBuf::from("staging"),
            files: [
                ("mods/a.jar", b"a".as_slice()),
                ("config/luckperms/lp.conf", b"lp".as_slice()),
                ("config/motd.conf", b"upstream-motd".as_slice()),
                ("world/level.dat", b"level".as_slice()),
                (".packctl.toml", b"evil".as_slice()),
            ]
            .into_iter()
            .map(|(rel, data)| PreparedFile {
                rel_path: PathBuf::from(rel),
                size: data.len() as u64,
                sha256: sha256_bytes(data),
            })
            .collect(),
            requirements: None,
            notices: Vec::new(),
        };
        let overlay = vec![OverlayFile {
            rel_path: PathBuf::from("config/motd.conf"),
            source: PathBuf::from("overlay/config/motd.conf"),
            sha256: sha256_bytes(b"overlay-motd"),
            size: 12,
        }];
        let ignore = crate::core::ignore::parse_rules("config/luckperms\n").unwrap();

        let (managed, shadowed) = managed_files_for(
            &FilePolicy::default_policy(),
            &prepared,
            &overlay,
            &ignore,
            &ReservedPaths::default_layout(),
            &InstalledState::default(),
        );

        assert_eq!(
            managed.keys().collect::<Vec<_>>(),
            vec!["mods/a.jar"],
            "ignored, persistent, reserved and overlay paths are not managed"
        );
        assert_eq!(shadowed.len(), 1);
        assert_eq!(
            shadowed["config/motd.conf"],
            fingerprint(b"upstream-motd"),
            "the shadowed record keeps the upstream fingerprint"
        );
    }

    #[tokio::test]
    async fn validation_failure_blocks_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();

        let overlay = tmp.path().join("overlay");
        let profile = profile(&server_root, &overlay);
        let controller = FakeBrokenController::new();
        let updater = Updater::new(
            profile,
            Box::new(FakeProvider::single()),
            Box::new(controller),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        let err = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap_err();
        match err {
            PackError::Validation(message) => {
                assert!(
                    message.contains("controller is not usable"),
                    "message: {message}"
                );
                assert!(
                    !message.contains("Rollback snapshot:"),
                    "preflight errors must not create a rollback snapshot: {message}"
                );
            }
            other => panic!("expected Validation error, got {other:?}"),
        }

        assert!(!server_root.join(".packctl").join("state.json").exists());
        assert_eq!(
            snapshot_count(&server_root),
            0,
            "preflight validation runs before stopping or snapshotting"
        );
    }

    #[test]
    fn from_profile_builds_provider_and_controller() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = profile(&tmp.path().join("server"), &tmp.path().join("overlay"));

        let updater = Updater::from_profile(&profile).unwrap();

        assert_eq!(updater.profile.name, "test-server");
        assert_eq!(updater.pack_ref().project_id, "42");
        assert!(updater.pack_ref().slug.is_empty());
    }

    #[tokio::test]
    async fn from_profile_builds_local_archive_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let mut profile = profile(&tmp.path().join("server"), &tmp.path().join("overlay"));
        profile.pack = PackSection::local(Some(PathBuf::from("/packs/server-pack.zip")));

        let updater = Updater::from_profile(&profile).unwrap();

        let err = updater
            .provider
            .list_versions(&updater.pack_ref())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("/packs/server-pack.zip"),
            "expected the local provider to fail on the missing archive, got: {err}"
        );
    }

    #[tokio::test]
    async fn from_profile_local_without_archive_defaults_to_packs_drop_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        let mut profile = profile(&server_root, &tmp.path().join("overlay"));
        profile.pack = PackSection::local(None);

        let updater = Updater::from_profile(&profile).unwrap();

        // The drop folder does not exist yet, so listing versions points the
        // error at the default <server root>/packs path.
        let err = updater
            .provider
            .list_versions(&updater.pack_ref())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("packs"),
            "expected the local provider to default to the packs drop folder, got: {err}"
        );
    }

    /// A provider whose prepared pack carries notices and requirements.
    struct NoticeProvider;

    #[async_trait::async_trait]
    impl PackProvider for NoticeProvider {
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
                requirements: Some(notice_requirements()),
                notices: vec!["Needs NeoForge".to_string()],
            })
        }
    }

    fn notice_requirements() -> crate::providers::PackRequirements {
        crate::providers::PackRequirements {
            minecraft: Some("1.21.1".to_string()),
            loader: Some(crate::providers::LoaderRequirement {
                kind: crate::providers::LoaderKind::NeoForge,
                version: "21.1.77".to_string(),
            }),
        }
    }

    #[tokio::test]
    async fn prepared_notices_and_requirements_flow_into_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        // The server already runs the loader the pack needs.
        std::fs::create_dir_all(server_root.join("libraries/net/neoforged/neoforge/21.1.77"))
            .unwrap();
        let profile = profile(&server_root, &tmp.path().join("overlay"));
        let updater = Updater::new(
            profile,
            Box::new(NoticeProvider),
            Box::new(FakeController::new()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();

        assert_eq!(
            prepared.plan.notices,
            vec![PlanNotice {
                path: None,
                verbose_only: false,
                message: "Needs NeoForge".to_string(),
            }]
        );
        assert_eq!(prepared.plan.requirements, Some(notice_requirements()));
        // The target's Minecraft version is recorded with the state.
        assert_eq!(prepared.next_state().minecraft.as_deref(), Some("1.21.1"));
    }

    #[tokio::test]
    async fn loader_mismatch_becomes_a_plan_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("libraries/net/neoforged/neoforge/21.1.50"))
            .unwrap();
        let profile = profile(&server_root, &tmp.path().join("overlay"));
        let updater = Updater::new(
            profile,
            Box::new(NoticeProvider),
            Box::new(FakeController::new()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        let messages: Vec<&str> = prepared
            .plan
            .notices
            .iter()
            .map(|n| n.message.as_str())
            .collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(
            messages[1].starts_with(
                "Loader mismatch\n  Pack requires NeoForge 21.1.77; server has NeoForge 21.1.50."
            ),
            "{messages:?}"
        );
    }

    fn listed(id: &str, released: &str, minecraft: &[&str]) -> PackVersion {
        PackVersion {
            id: id.to_string(),
            name: format!("Pack {id}"),
            released: Some(released.to_string()),
            game_versions: minecraft.iter().map(|v| v.to_string()).collect(),
            ..Default::default()
        }
    }

    fn installed(id: &str, minecraft: Option<&str>) -> InstalledState {
        InstalledState {
            installed_version: Some(format!("Pack {id}")),
            provider_version_id: Some(id.to_string()),
            minecraft: minecraft.map(str::to_string),
            ..InstalledState::default()
        }
    }

    /// Newest first, like providers return them: a 26.3 beta line, a 26.2
    /// backport published after the 26.2 main release, and older versions.
    fn parallel_lines() -> Vec<PackVersion> {
        vec![
            listed("15.0.0", "2026-03-02T10:00:00Z", &["26.3"]),
            listed("14.1.2", "2026-03-02T09:50:00Z", &["26.2"]),
            listed("14.1.1", "2026-02-01T00:00:00Z", &["26.2"]),
            listed("13.6.0", "2026-03-02T09:40:00Z", &["26.1.2"]),
            listed("14.0.0", "2026-01-01T00:00:00Z", &["26.2"]),
        ]
    }

    #[test]
    fn latest_stays_on_the_installed_minecraft_version() {
        let versions = parallel_lines();
        let choice = latest_compatible(&versions, &installed("14.1.1", None))
            .unwrap()
            .unwrap();
        assert_eq!(choice.id, "14.1.2");
        assert_eq!(
            choice.notice.as_deref(),
            Some("Newer version Pack 15.0.0 targets Minecraft 26.3; use --to to switch")
        );

        // The recorded Minecraft version wins over the listing.
        let choice = latest_compatible(&versions, &installed("14.1.1", Some("26.1.2")))
            .unwrap()
            .unwrap();
        assert_eq!(choice.id, "13.6.0");

        // Already on the newest version: no notice.
        let choice = latest_compatible(&versions, &installed("15.0.0", None))
            .unwrap()
            .unwrap();
        assert_eq!(choice.id, "15.0.0");
        assert_eq!(choice.notice, None);
    }

    #[test]
    fn latest_never_goes_back_in_time() {
        // A beta newer than the newest release: "latest" (the release sorted
        // first) would be a downgrade, so the installed beta stays.
        let versions = vec![
            listed("release", "2026-01-01T00:00:00Z", &[]),
            listed("beta", "2026-02-01T00:00:00Z", &[]),
        ];
        let choice = latest_compatible(&versions, &installed("beta", None))
            .unwrap()
            .unwrap();
        assert_eq!(choice.id, "beta");
    }

    #[test]
    fn latest_is_unrestricted_without_installed_metadata() {
        let versions = parallel_lines();
        assert_eq!(
            latest_compatible(&versions, &installed("gone", None)).unwrap(),
            None
        );
        // Only versions for another Minecraft version: an explicit error.
        let err =
            latest_compatible(&versions[..1], &installed("gone", Some("1.20.1"))).unwrap_err();
        assert!(
            matches!(&err, PackError::NotFound(m) if m.contains("Minecraft 1.20.1") && m.contains("--to")),
            "{err:?}"
        );
    }

    /// Lists [`parallel_lines`] and resolves ids like a real provider.
    struct ParallelProvider;

    #[async_trait::async_trait]
    impl PackProvider for ParallelProvider {
        async fn list_versions(&self, _pack: &PackRef) -> Result<Vec<PackVersion>> {
            Ok(parallel_lines())
        }

        async fn resolve_version(
            &self,
            pack: &PackRef,
            selector: &VersionSelector,
        ) -> Result<ResolvedPackVersion> {
            let versions = parallel_lines();
            let version = crate::providers::resolve_version_from_list(&versions, selector, "test")?;
            Ok(ResolvedPackVersion {
                pack: pack.clone(),
                version: version.clone(),
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
                name: "parallel".to_string(),
                version: version.version.clone(),
                root: server_root,
                files: Vec::new(),
                requirements: None,
                notices: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn prepare_update_latest_keeps_minecraft_version() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();
        let profile = profile(&server_root, &tmp.path().join("overlay"));
        StateStore::at(&server_root)
            .unwrap()
            .save(&InstalledState {
                provider: Some(profile.pack.provider.as_str().to_string()),
                project: StateSource::for_profile(&profile).project,
                ..installed("14.1.1", None)
            })
            .unwrap();
        let updater = Updater::new(
            profile,
            Box::new(ParallelProvider),
            Box::new(FakeController::new()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert_eq!(prepared.plan.to_id, "14.1.2");
        assert!(
            prepared.plan.notices.iter().any(|n| n
                .message
                .starts_with("Newer version Pack 15.0.0 targets Minecraft 26.3")),
            "{:?}",
            prepared.plan.notices
        );
        assert_eq!(prepared.next_state().minecraft.as_deref(), Some("26.2"));

        // An explicit target is honoured as asked.
        let prepared = updater
            .prepare_update(&VersionSelector::Query("15.0.0".to_string()))
            .await
            .unwrap();
        assert_eq!(prepared.plan.to_id, "15.0.0");
    }

    #[tokio::test]
    async fn provider_change_adds_notice_and_clears_from_id() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();
        // Recorded from a local archive; the profile now follows CurseForge
        // project 42. The local version id happens to equal the new one.
        write_state(
            &server_root,
            &InstalledState {
                installed_version: Some("2.0".to_string()),
                provider_version_id: Some("v2".to_string()),
                provider: Some("local".to_string()),
                last_successful_update: Some(Utc::now()),
                ..InstalledState::default()
            },
        );
        let profile = profile(&server_root, &tmp.path().join("overlay"));
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

        assert_eq!(prepared.plan.from_id, None);
        assert_eq!(prepared.plan.from_version.as_deref(), Some("2.0"));
        assert_eq!(
            prepared.plan.notices,
            vec![PlanNotice {
                path: None,
                verbose_only: false,
                message: "Provider changed\n  Recorded state came from local; this profile now \
                          follows curseforge project 42.\n  Files are compared by content; \
                          version ids are not comparable."
                    .to_string(),
            }]
        );
        // The same version id from another provider is not "already
        // installed": the new source is recorded.
        assert!(prepared.plan.is_empty());
        assert!(prepared.needs_state_commit());
        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(outcome.committed);
        assert!(controller.log().is_empty());

        let state = load_state(&server_root);
        assert_eq!(state.provider.as_deref(), Some("curseforge"));
        assert_eq!(state.project.as_deref(), Some("42"));
        assert_eq!(state.provider_version_id.as_deref(), Some("v2"));

        // Once recorded, the same source plans without the notice.
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(prepared.plan.notices.is_empty(), "{:?}", prepared.plan);
        assert_eq!(prepared.plan.from_id.as_deref(), Some("v2"));
        assert!(!prepared.needs_state_commit());
    }

    #[test]
    fn state_source_ignores_state_without_provider_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = profile(&tmp.path().join("server"), &tmp.path().join("overlay"));
        let source = StateSource::for_profile(&profile);
        assert_eq!(source.provider, "curseforge");
        assert_eq!(source.project.as_deref(), Some("42"));

        assert!(!source.differs_from(&InstalledState::default()));
        let other_project = InstalledState {
            provider: Some("curseforge".to_string()),
            project: Some("43".to_string()),
            ..InstalledState::default()
        };
        assert!(source.differs_from(&other_project));

        let mut local = profile.clone();
        local.pack = PackSection::local(None);
        let source = StateSource::for_profile(&local);
        assert_eq!(source.provider, "local");
        assert_eq!(source.project, None);
    }

    /// Pack 1.0 and 2.0 for the adoption tests; 2.0 changes `mods/a.jar`
    /// and `config/reformatted.toml`.
    fn adoption_provider() -> FakeProvider {
        FakeProvider::new(&[
            (
                "id-2",
                "2.0",
                &[
                    ("mods/a.jar", b"a-v2".as_slice()),
                    ("mods/b.jar", b"b".as_slice()),
                    ("config/edited.toml", b"x = 1\n".as_slice()),
                    ("config/reformatted.toml", b"[s]\nlimit = 8\n".as_slice()),
                ],
            ),
            (
                "id-1",
                "1.0",
                &[
                    ("mods/a.jar", b"a-v1".as_slice()),
                    ("mods/b.jar", b"b".as_slice()),
                    ("config/edited.toml", b"x = 1\n".as_slice()),
                    ("config/reformatted.toml", b"[s]\nlimit = 4\n".as_slice()),
                ],
            ),
        ])
    }

    /// A hand-installed 1.0 server with an added mod, an edited config, a
    /// reformatted config, an added config directory and runtime data.
    fn hand_installed_v1(server_root: &Path) {
        crate::testutil::write_files(
            server_root,
            &[
                ("mods/a.jar", b"a-v1"),
                ("mods/b.jar", b"b"),
                ("mods/added.jar", b"admin mod"),
                ("config/edited.toml", b"x = 2\n"),
                ("config/reformatted.toml", b"# mine\n[s]\nlimit = 4  \n"),
                ("config/luckperms/luckperms.conf", b"lp"),
                ("config/luckperms/data.mv.db", b"players"),
                ("world/level.dat", b"world"),
            ],
        );
    }

    /// Prepares and applies an adoption of `version`.
    async fn adopt(
        updater: &Updater,
        version: &str,
        opts: &AdoptOptions,
    ) -> crate::core::adopt::AdoptOutcome {
        let adoption = updater
            .prepare_adoption(&VersionSelector::Query(version.to_string()), opts)
            .await
            .unwrap();
        crate::core::adopt::apply_adoption(
            &adoption.report,
            &adoption.prepared,
            &updater.profile,
            &adoption.overlay_files,
            &crate::core::adopt::ApplyOptions::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn adopt_then_plan_same_version_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        hand_installed_v1(&server_root);
        let controller = FakeController::stopped();
        let updater = Updater::new(
            profile(&server_root, &server_root.join("overlay")),
            Box::new(adoption_provider()),
            Box::new(controller.clone()),
        );

        // Before adopting, the first update would destroy admin changes.
        let before = updater
            .prepare_update(&VersionSelector::Query("1.0".to_string()))
            .await
            .unwrap();
        assert!(before.first_update_risk().is_some());

        adopt(&updater, "1.0", &AdoptOptions::default()).await;

        let after = updater
            .prepare_update(&VersionSelector::Query("1.0".to_string()))
            .await
            .unwrap();
        assert!(after.plan.is_empty(), "{:#?}", after.plan);
        assert!(after.plan.unmanaged_removals.is_empty());
        assert!(!after.needs_force_sync());
        assert!(
            !after.needs_state_commit(),
            "the adopted state is exactly what an update would record"
        );
        let outcome = updater
            .execute(&after, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(!outcome.committed);
        assert!(controller.log().is_empty());
    }

    #[tokio::test]
    async fn readopting_keeps_the_previous_blocks_protections() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        hand_installed_v1(&server_root);
        let updater = Updater::new(
            profile(&server_root, &server_root.join("overlay")),
            Box::new(adoption_provider()),
            Box::new(FakeController::stopped()),
        );
        let opts = AdoptOptions {
            modified: crate::core::adopt::ModifiedPolicy::Ignore,
            ..AdoptOptions::default()
        };
        adopt(&updater, "1.0", &opts).await;

        let plan_is_empty = |label: &'static str| {
            let updater = &updater;
            async move {
                let update = updater
                    .prepare_update(&VersionSelector::Query("1.0".to_string()))
                    .await
                    .unwrap();
                assert!(update.plan.is_empty(), "{label}: {:#?}", update.plan);
                assert!(update.plan.unmanaged_removals.is_empty(), "{label}");
            }
        };
        plan_is_empty("first adoption").await;

        // State lost while .packctlignore stayed: a plain adopt again.
        std::fs::remove_file(server_root.join(STATE_REL_PATH)).unwrap();
        let again = updater
            .prepare_adoption(&VersionSelector::Query("1.0".to_string()), &opts)
            .await
            .unwrap();
        assert!(
            again
                .report
                .ignore_rules
                .contains(&"config/edited.toml".to_string()),
            "{:#?}",
            again.report.ignore_rules
        );
        assert!(
            again
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("already has a 'packctl adopt' block")),
            "{:#?}",
            again.report.warnings
        );
        crate::core::adopt::apply_adoption(
            &again.report,
            &again.prepared,
            &updater.profile,
            &again.overlay_files,
            &crate::core::adopt::ApplyOptions::default(),
        )
        .unwrap();
        plan_is_empty("after the state was lost").await;

        // `adopt --force` over existing state.
        let forced = updater
            .prepare_adoption(&VersionSelector::Query("1.0".to_string()), &opts)
            .await
            .unwrap();
        crate::core::adopt::apply_adoption(
            &forced.report,
            &forced.prepared,
            &updater.profile,
            &forced.overlay_files,
            &crate::core::adopt::ApplyOptions { force: true },
        )
        .unwrap();
        plan_is_empty("after --force").await;
        assert_eq!(
            std::fs::read(server_root.join("config/edited.toml")).unwrap(),
            b"x = 2\n"
        );
    }

    #[tokio::test]
    async fn adopt_with_missing_files_and_upstream_policy_plans_only_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        hand_installed_v1(&server_root);
        // A config the admin deleted, and the edited config restored to the
        // pack's version by policy.
        std::fs::remove_file(server_root.join("config/reformatted.toml")).unwrap();
        let updater = Updater::new(
            profile(&server_root, &server_root.join("overlay")),
            Box::new(adoption_provider()),
            Box::new(FakeController::stopped()),
        );
        let opts = AdoptOptions {
            modified: crate::core::adopt::ModifiedPolicy::Upstream,
            ..AdoptOptions::default()
        };
        adopt(&updater, "1.0", &opts).await;

        let plan = updater
            .prepare_update(&VersionSelector::Query("1.0".to_string()))
            .await
            .unwrap()
            .plan;
        let paths = |changes: &[crate::core::planner::FileChange]| -> Vec<PathBuf> {
            changes
                .iter()
                .map(|change| change.rel_path.clone())
                .collect()
        };
        assert_eq!(
            paths(&plan.additions),
            vec![PathBuf::from("config/reformatted.toml")],
            "the missing config is restored"
        );
        assert_eq!(
            paths(&plan.modifications),
            vec![PathBuf::from("config/edited.toml")],
            "the upstream policy writes the pack version back"
        );
        assert!(plan.removals.is_empty(), "{:#?}", plan.removals);
        assert_eq!(plan.pending_overlay_count(), 0);
        assert!(!plan.needs_force_sync());
    }

    #[tokio::test]
    async fn adopt_then_update_v2_preserves_added_mods_edited_configs_and_ignored_data() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        hand_installed_v1(&server_root);
        let controller = FakeController::stopped();
        let updater = Updater::new(
            profile(&server_root, &server_root.join("overlay")),
            Box::new(adoption_provider()),
            Box::new(controller.clone()),
        );
        adopt(&updater, "1.0", &AdoptOptions::default()).await;

        let v2 = updater
            .prepare_update(&VersionSelector::Query("2.0".to_string()))
            .await
            .unwrap();
        assert!(v2.plan.removals.is_empty(), "{:#?}", v2.plan.removals);
        assert!(!v2.needs_force_sync());
        let outcome = updater
            .execute(&v2, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(outcome.committed);

        let read = |rel: &str| std::fs::read(server_root.join(rel)).unwrap();
        assert_eq!(read("mods/a.jar"), b"a-v2", "the pack update applied");
        assert_eq!(read("mods/added.jar"), b"admin mod");
        assert_eq!(read("config/edited.toml"), b"x = 2\n");
        assert_eq!(read("config/luckperms/data.mv.db"), b"players");
        assert_eq!(read("config/luckperms/luckperms.conf"), b"lp");
        assert_eq!(read("world/level.dat"), b"world");
        // Upstream changed the reformatted config, so the new version wins.
        assert_eq!(read("config/reformatted.toml"), b"[s]\nlimit = 8\n");

        let state = load_state(&server_root);
        assert_eq!(state.provider_version_id.as_deref(), Some("id-2"));
        assert!(state.adopted.is_some(), "the adoption record is kept");
    }

    #[tokio::test]
    async fn local_sha256_alias_suppresses_replace_until_upstream_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        hand_installed_v1(&server_root);
        let updater = Updater::new(
            profile(&server_root, &server_root.join("overlay")),
            Box::new(adoption_provider()),
            Box::new(FakeController::stopped()),
        );
        let outcome = adopt(&updater, "1.0", &AdoptOptions::default()).await;
        let record = &outcome.state.managed_files["config/reformatted.toml"];
        assert_eq!(record.sha256, sha256_bytes(b"[s]\nlimit = 4\n"));
        assert_eq!(
            record.local_sha256.as_deref(),
            Some(sha256_bytes(b"# mine\n[s]\nlimit = 4  \n").as_str())
        );

        // Same upstream: the reformatted live file is left alone, and the
        // alias survives a state refresh.
        let same = updater
            .prepare_update(&VersionSelector::Query("1.0".to_string()))
            .await
            .unwrap();
        assert!(same.plan.modifications.is_empty());
        assert_eq!(
            same.next_state().managed_files["config/reformatted.toml"].local_sha256,
            record.local_sha256
        );

        // Upstream changed the file: it is replaced and the alias dropped.
        let v2 = updater
            .prepare_update(&VersionSelector::Query("2.0".to_string()))
            .await
            .unwrap();
        assert!(
            v2.plan
                .modifications
                .iter()
                .any(|change| change.rel_path == Path::new("config/reformatted.toml"))
        );
        assert_eq!(
            v2.next_state().managed_files["config/reformatted.toml"].local_sha256,
            None
        );
    }

    #[tokio::test]
    async fn adopted_server_refuses_overwriting_a_hand_added_file() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        crate::testutil::write_files(&server_root, &[("mods/a.jar", b"a-v1")]);
        let provider = FakeProvider::new(&[
            (
                "id-2",
                "2.0",
                &[
                    ("mods/a.jar", b"a-v1".as_slice()),
                    ("config/y-common.toml", b"pack default".as_slice()),
                ],
            ),
            ("id-1", "1.0", &[("mods/a.jar", b"a-v1".as_slice())]),
        ]);
        let controller = FakeController::stopped();
        let updater = Updater::new(
            profile(&server_root, &server_root.join("overlay")),
            Box::new(provider),
            Box::new(controller.clone()),
        );
        adopt(&updater, "1.0", &AdoptOptions::default()).await;
        // After adoption the admin adds a mod by hand and tunes its config.
        crate::testutil::write_files(
            &server_root,
            &[("config/y-common.toml", b"tuned by the admin")],
        );

        let v2 = updater
            .prepare_update(&VersionSelector::Query("2.0".to_string()))
            .await
            .unwrap();
        assert_eq!(
            v2.plan.unmanaged_overwrites,
            vec![PathBuf::from("config/y-common.toml")]
        );
        assert!(v2.needs_force_sync());

        let err = updater
            .execute(&v2, &ExecuteOptions::default())
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("overwrite 1 file(s) packctl never installed"),
            "{message}"
        );
        assert!(message.contains("config/y-common.toml"), "{message}");
        assert_eq!(
            std::fs::read(server_root.join("config/y-common.toml")).unwrap(),
            b"tuned by the admin"
        );
        assert!(controller.log().is_empty());

        let outcome = updater
            .execute(&v2, &ExecuteOptions::force_sync())
            .await
            .unwrap();
        assert!(outcome.committed);
        assert_eq!(
            std::fs::read(server_root.join("config/y-common.toml")).unwrap(),
            b"pack default"
        );
    }

    #[tokio::test]
    async fn drift_written_on_shutdown_is_refused_and_the_server_restarted() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("config")).unwrap();
        std::fs::write(server_root.join("config/quests.snbt"), b"adopted").unwrap();
        record_v2(&server_root, &[("config/quests.snbt", b"adopted")]);
        let overlay = tmp.path().join("overlay");
        crate::testutil::write_files(&overlay, &[("config/quests.snbt", b"adopted")]);

        // In sync while the plan is made; the server saves progress as it stops.
        let controller = FakeController::writing_on_stop(
            server_root.join("config/quests.snbt"),
            b"progress saved on shutdown",
        );
        let updater = Updater::new(
            profile(&server_root, &overlay),
            Box::new(FakeProvider::single()),
            Box::new(controller.clone()),
        );
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(!prepared.plan.is_empty());
        assert!(!prepared.needs_force_sync());

        let err = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with("files changed while the server stopped"),
            "{message}"
        );
        assert!(message.contains("config/quests.snbt"), "{message}");
        assert_eq!(controller.log(), vec!["stop", "start"]);
        assert_eq!(controller.status_value(), ServerStatus::Running);
        assert_eq!(
            std::fs::read(server_root.join("config/quests.snbt")).unwrap(),
            b"progress saved on shutdown"
        );
        assert!(!server_root.join("mods/upstream.jar").exists());
        assert_eq!(snapshot_count(&server_root), 0);
        assert_eq!(
            load_state(&server_root).installed_version.as_deref(),
            Some("2.0")
        );
    }

    #[tokio::test]
    async fn state_changed_after_prepare_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(&server_root).unwrap();
        record_v2(&server_root, &[]);
        let original = std::fs::read(server_root.join(".packctl/state.json")).unwrap();
        let controller = FakeController::new();
        let updater = Updater::new(
            profile(&server_root, &tmp.path().join("overlay")),
            Box::new(FakeProvider::single()),
            Box::new(controller.clone()),
        );
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert!(!prepared.plan.is_empty());

        // Another command (an adopt, a rollback) records something else
        // while this update waits at its prompt.
        let mut other = load_state(&server_root);
        other.installed_version = Some("1.0".to_string());
        write_state(&server_root, &other);
        let err = updater
            .execute(&prepared, &ExecuteOptions::force_sync())
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("changed since the plan was prepared"),
            "{err}"
        );
        assert_eq!(load_state(&server_root), other);

        // So does a new .packctlignore rule.
        std::fs::write(server_root.join(".packctl/state.json"), &original).unwrap();
        std::fs::write(server_root.join(".packctlignore"), "mods/upstream.jar\n").unwrap();
        let err = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("changed since the plan was prepared"),
            "{err}"
        );
        assert!(controller.log().is_empty());
        assert!(!server_root.join("mods/upstream.jar").exists());
        assert_eq!(snapshot_count(&server_root), 0);

        // Unchanged again, the plan runs.
        std::fs::remove_file(server_root.join(".packctlignore")).unwrap();
        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(outcome.committed);
    }

    #[tokio::test]
    async fn first_update_guard_refuses_without_force_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        hand_installed_v1(&server_root);
        // Only edits, no extra files: the guard still fires for overwrites.
        std::fs::remove_file(server_root.join("mods/added.jar")).unwrap();
        std::fs::remove_dir_all(server_root.join("config/luckperms")).unwrap();
        let before = crate::testutil::tree_snapshot(&server_root);
        let controller = FakeController::new();
        let updater = Updater::new(
            profile(&server_root, &tmp.path().join("overlay")),
            Box::new(adoption_provider()),
            Box::new(controller.clone()),
        );

        let prepared = updater
            .prepare_update(&VersionSelector::Query("1.0".to_string()))
            .await
            .unwrap();
        assert!(prepared.plan.unmanaged_removals.is_empty());
        assert!(!prepared.plan.needs_force_sync());
        let risk = prepared.first_update_risk().unwrap();
        assert_eq!(risk.removals, 0);
        assert_eq!(risk.overwrites, 2);

        let err = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap_err();
        let PackError::Config(message) = &err else {
            panic!("expected a Config error, got {err:?}");
        };
        assert!(
            message.starts_with("refusing the first update of 'test-server'"),
            "{message}"
        );
        assert!(
            message.contains("packctl adopt --as <version>"),
            "{message}"
        );
        assert!(message.contains("config/edited.toml"), "{message}");
        assert!(message.contains("--force-sync"), "{message}");
        assert!(controller.log().is_empty());
        assert_eq!(crate::testutil::tree_snapshot(&server_root), before);

        // The hint names the profile the user typed.
        let named = ExecuteOptions {
            adopt_command: Some("packctl adopt atm10".to_string()),
            ..ExecuteOptions::default()
        };
        let err = updater.execute(&prepared, &named).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("packctl adopt atm10 --as <version>"),
            "{err}"
        );

        let outcome = updater
            .execute(&prepared, &ExecuteOptions::force_sync())
            .await
            .unwrap();
        assert!(outcome.committed);
        assert_eq!(
            std::fs::read(server_root.join("config/edited.toml")).unwrap(),
            b"x = 1\n"
        );
    }

    #[tokio::test]
    async fn first_update_guard_allows_empty_server() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        std::fs::create_dir_all(server_root.join("world")).unwrap();
        std::fs::write(server_root.join("world/level.dat"), b"world").unwrap();
        let updater = Updater::new(
            profile(&server_root, &tmp.path().join("overlay")),
            Box::new(adoption_provider()),
            Box::new(FakeController::new()),
        );
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        assert_eq!(prepared.first_update_risk(), None);
        assert!(!prepared.needs_force_sync());
        let outcome = updater
            .execute(&prepared, &ExecuteOptions::default())
            .await
            .unwrap();
        assert!(outcome.committed);
        assert_eq!(outcome.upstream_writes, 4);
    }

    #[tokio::test]
    async fn duplicate_mod_id_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let jars = tmp.path().join("jars");
        crate::core::modid::write_mod_jar(&jars.join("pack.jar"), "jei", "pack");
        crate::core::modid::write_mod_jar(&jars.join("overlay.jar"), "jei", "overlay");
        crate::core::modid::write_mod_jar(&jars.join("kept.jar"), "create", "kept");
        crate::core::modid::write_mod_jar(&jars.join("create.jar"), "create", "pack");
        let read = |name: &str| std::fs::read(jars.join(name)).unwrap();
        let (pack_jei, overlay_jei, kept, create) = (
            read("pack.jar"),
            read("overlay.jar"),
            read("kept.jar"),
            read("create.jar"),
        );
        let provider = FakeProvider::new(&[(
            "id-1",
            "1.0",
            &[
                ("mods/jei-15.2.jar", pack_jei.as_slice()),
                ("mods/create-0.5.jar", create.as_slice()),
            ],
        )]);

        let server_root = tmp.path().join("server");
        let overlay = server_root.join("overlay");
        crate::testutil::write_files(
            &server_root,
            &[
                ("overlay/mods/jei-15.3.jar", overlay_jei.as_slice()),
                ("mods/create-old.jar", kept.as_slice()),
                (".packctlignore", b"mods/create-old.jar\n"),
            ],
        );
        let updater = Updater::new(
            profile(&server_root, &overlay),
            Box::new(provider),
            Box::new(FakeController::new()),
        );
        let prepared = updater
            .prepare_update(&VersionSelector::Latest)
            .await
            .unwrap();
        let messages: Vec<&str> = prepared
            .plan
            .notices
            .iter()
            .map(|notice| notice.message.as_str())
            .filter(|message| message.starts_with("Duplicate mod id"))
            .collect();
        assert_eq!(
            messages,
            vec![
                "Duplicate mod id 'create'\n  mods/create-0.5.jar (upstream)\n  \
                 mods/create-old.jar (kept)\n  The server will likely refuse to start; remove one \
                 copy from the overlay or .packctlignore.",
                "Duplicate mod id 'jei'\n  mods/jei-15.2.jar (upstream)\n  \
                 mods/jei-15.3.jar (overlay)\n  The server will likely refuse to start; remove one \
                 copy from the overlay or .packctlignore.",
            ]
        );
    }

    #[tokio::test]
    async fn detect_version_picks_the_installed_version() {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = tmp.path().join("server");
        crate::testutil::write_files(
            &server_root,
            &[
                ("mods/a.jar", b"a-v1"),
                ("mods/old.jar", b"old"),
                ("mods/admin.jar", b"admin"),
            ],
        );
        let updater = Updater::new(
            profile(&server_root, &tmp.path().join("overlay")),
            Box::new(FakeProvider::two_versions()),
            Box::new(FakeController::new()),
        );
        match updater.detect_version().await.unwrap() {
            Detection::Picked { version, score } => {
                assert_eq!(version.id, "id-1");
                assert_eq!(score, 1.0);
            }
            other => panic!("expected a pick, got {other:?}"),
        }

        // Half of each version matches: no confident pick.
        std::fs::write(server_root.join("mods/a.jar"), b"a-v2").unwrap();
        std::fs::remove_file(server_root.join("mods/old.jar")).unwrap();
        assert!(matches!(
            updater.detect_version().await.unwrap(),
            Detection::Ambiguous(_)
        ));
    }
}
