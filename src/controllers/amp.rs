//! CubeCoders AMP server controller.
//!
//! Lifecycle operations are delegated to the `ampinstmgr` CLI. AMP-specific
//! commands stay in this module; core update code only sees the
//! [`crate::controllers::ServerController`] interface.

use std::process::Output;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::config::profile::{ControllerKind, ControllerSection};
use crate::controllers::{ServerController, ServerStatus};
use crate::error::{PackError, Result};

/// Path/binary used to control AMP instances.
const DEFAULT_AMP_INSTMGR: &str = "ampinstmgr";

/// Default timeout for every `ampinstmgr` invocation and confirmation poll.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Poll interval while waiting for a lifecycle transition.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A CubeCoders AMP controller wrapping the `ampinstmgr` CLI.
#[derive(Debug)]
pub struct AmpController {
    pub ampinstmgr: String,
    pub instance: String,
    pub timeout_ms: u64,
}

impl AmpController {
    /// Build a controller for `instance` using the `ampinstmgr` binary on PATH
    /// and the default timeout.
    pub fn new(instance: String) -> Self {
        Self::with_binary(
            DEFAULT_AMP_INSTMGR.to_string(),
            instance,
            DEFAULT_TIMEOUT_MS,
        )
    }

    pub fn with_binary(ampinstmgr: String, instance: String, timeout_ms: u64) -> Self {
        AmpController {
            ampinstmgr,
            instance,
            timeout_ms,
        }
    }

    /// Build a controller from a profile `[controller]` section.
    ///
    /// Requires the `amp` controller kind and a non-empty `instance`. The
    /// When a command configuration is present, its timeout is also used for
    /// AMP lifecycle calls; otherwise AMP uses [`DEFAULT_TIMEOUT_MS`].
    pub fn from_profile(controller: &ControllerSection) -> Result<Self> {
        if controller.kind != ControllerKind::Amp {
            return Err(PackError::Controller(format!(
                "expected an 'amp' controller section, got {:?}",
                controller.kind
            )));
        }
        let Some(instance) = controller.instance.as_deref() else {
            return Err(PackError::Controller(
                "amp controller requires an 'instance' in the profile".into(),
            ));
        };
        if instance.is_empty() {
            return Err(PackError::Controller(
                "amp controller requires a non-empty 'instance'".into(),
            ));
        }
        Ok(AmpController::with_binary(
            DEFAULT_AMP_INSTMGR.to_string(),
            instance.to_string(),
            controller
                .command
                .as_ref()
                .and_then(|command| command.timeout_ms)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
        ))
    }

    /// `ampinstmgr status <instance>`
    fn status_args(&self) -> Vec<String> {
        vec![
            self.ampinstmgr.clone(),
            "status".into(),
            self.instance.clone(),
        ]
    }

    /// `ampinstmgr stop <instance> --wait`
    ///
    /// `--wait` makes `ampinstmgr` block until the instance has actually
    /// stopped, so a successful exit already verifies the server stopped.
    fn stop_args(&self) -> Vec<String> {
        vec![
            self.ampinstmgr.clone(),
            "stop".into(),
            self.instance.clone(),
            "--wait".into(),
        ]
    }

    /// `ampinstmgr start <instance>`
    fn start_args(&self) -> Vec<String> {
        vec![
            self.ampinstmgr.clone(),
            "start".into(),
            self.instance.clone(),
        ]
    }

    /// Run `argv` without a shell, bounded by `timeout_ms`.
    async fn run_command(&self, argv: &[String], what: &str) -> Result<Output> {
        crate::controllers::run_argv_bounded(
            argv,
            Duration::from_millis(self.timeout_ms),
            what,
            "ensure the ampinstmgr binary exists and is executable",
        )
        .await
    }

    /// Wait for `status()` to report `target`, polling every ~1s up to
    /// `timeout_ms`. [`ServerStatus::Unknown`] keeps polling.
    async fn wait_until(&self, target: ServerStatus) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);

        loop {
            if Instant::now() >= deadline {
                return Err(PackError::Controller(format!(
                    "AMP instance '{}' did not reach {target:?} within {} ms",
                    self.instance, self.timeout_ms
                )));
            }
            match self.status().await? {
                current if current == target => return Ok(()),
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }
}

/// Map `ampinstmgr status` output to a [`ServerStatus`].
///
/// AMP's status output is human-oriented text, so matching is substring-based
/// and case-insensitive.
fn parse_status_output(output: &str) -> ServerStatus {
    let text = output.to_lowercase();
    if text.contains("running") {
        ServerStatus::Running
    } else if text.contains("stopped") {
        ServerStatus::Stopped
    } else {
        ServerStatus::Unknown
    }
}

#[async_trait]
impl ServerController for AmpController {
    /// Report lifecycle state from `ampinstmgr status <instance>`.
    ///
    /// Exit code mapping is intentionally lenient: a zero exit means the
    /// instance was found and the printed state is authoritative; a non-zero
    /// exit does not reliably mean "stopped", so it maps to
    /// [`ServerStatus::Unknown`] rather than failing. A spawn failure (missing
    /// `ampinstmgr`) is a real error and is never hidden.
    async fn status(&self) -> Result<ServerStatus> {
        let output = self.run_command(&self.status_args(), "status").await?;
        if output.status.code() != Some(0) {
            return Ok(ServerStatus::Unknown);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(parse_status_output(&format!("{stdout}{stderr}")))
    }

    /// Stop the instance with `ampinstmgr stop <instance> --wait`, then confirm
    /// the transition to [`ServerStatus::Stopped`].
    ///
    /// `--wait` blocks until the instance reports stopped, which already
    /// satisfies "verify the server actually stopped before continuing". The
    /// follow-up status poll is a belt-and-braces confirmation.
    async fn stop(&self) -> Result<()> {
        let output = self.run_command(&self.stop_args(), "stop").await?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PackError::Controller(format!(
                "ampinstmgr stop failed for instance '{}' (exit {:?})\nstdout: {}\nstderr: {}",
                self.instance,
                output.status.code(),
                stdout.trim(),
                stderr.trim()
            )));
        }
        self.wait_until(ServerStatus::Stopped).await
    }

    async fn start(&self) -> Result<()> {
        let output = self.run_command(&self.start_args(), "start").await?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PackError::Controller(format!(
                "ampinstmgr start failed for instance '{}' (exit {:?})\nstdout: {}\nstderr: {}",
                self.instance,
                output.status.code(),
                stdout.trim(),
                stderr.trim()
            )));
        }
        self.wait_until(ServerStatus::Running).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn controller() -> AmpController {
        AmpController::new("ATM10".into())
    }

    /// Write `body` to an executable script inside `dir` and return its path.
    ///
    /// The script is written to a temporary sibling file and renamed into
    /// place so it is never executed while still open for writing (which would
    /// intermittently fail with ETXTBSY).
    fn write_executable(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        let tmp = dir.join(format!(".{name}.tmp"));
        {
            use std::io::Write as _;
            let mut file = fs::File::create(&tmp).unwrap();
            file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
                .unwrap();
            file.sync_all().unwrap();
        }
        fs::rename(&tmp, &path).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// A fake `ampinstmgr` whose instance state is a file: `status` prints
    /// "running" while the file exists and "stopped" otherwise; `stop` removes
    /// it; `start` creates it.
    fn fake_ampinstmgr(dir: &Path, state: &Path) -> String {
        write_executable(
            dir,
            "ampinstmgr",
            &format!(
                "case \"$1\" in\n\
                 status)\n  \
                   if [ -f {} ]; then echo \"Instance is running\"; else echo \"Instance is stopped\"; fi\n\
                   exit 0 ;;\n\
                 stop)\n  \
                   rm -f {}; echo \"Stopping instance...\"; exit 0 ;;\n\
                 start)\n  \
                   touch {}; echo \"Starting instance...\"; exit 0 ;;\n\
                 *)\n  \
                   echo \"unknown subcommand: $1\" >&2; exit 1 ;;\n\
                 esac",
                state.display(),
                state.display(),
                state.display()
            ),
        )
    }

    #[test]
    fn new_uses_default_binary_and_timeout() {
        let ctrl = controller();
        assert_eq!(ctrl.ampinstmgr, "ampinstmgr");
        assert_eq!(ctrl.instance, "ATM10");
        assert_eq!(ctrl.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn status_args_are_amp_status() {
        assert_eq!(
            controller().status_args(),
            ["ampinstmgr", "status", "ATM10"]
        );
    }

    #[test]
    fn stop_args_include_wait() {
        assert_eq!(
            controller().stop_args(),
            ["ampinstmgr", "stop", "ATM10", "--wait"]
        );
    }

    #[test]
    fn start_args_are_amp_start() {
        assert_eq!(controller().start_args(), ["ampinstmgr", "start", "ATM10"]);
    }

    #[test]
    fn with_binary_uses_given_binary() {
        let ctrl = AmpController::with_binary("/opt/amp/ampinstmgr".into(), "ATM10".into(), 5_000);
        assert_eq!(ctrl.ampinstmgr, "/opt/amp/ampinstmgr");
        assert_eq!(ctrl.timeout_ms, 5_000);
        assert_eq!(
            ctrl.status_args(),
            ["/opt/amp/ampinstmgr", "status", "ATM10"]
        );
    }

    #[test]
    fn parse_status_output_mapping() {
        assert_eq!(
            parse_status_output("Instance 'ATM10' is running"),
            ServerStatus::Running
        );
        assert_eq!(
            parse_status_output("Instance ATM10 is currently RUNNING."),
            ServerStatus::Running
        );
        assert_eq!(
            parse_status_output("Instance stopped"),
            ServerStatus::Stopped
        );
        assert_eq!(
            parse_status_output("Stopping instance 'ATM10'..."),
            ServerStatus::Unknown
        );
        assert_eq!(parse_status_output(""), ServerStatus::Unknown);
        assert_eq!(
            parse_status_output("Instance is restarting"),
            ServerStatus::Unknown
        );
    }

    #[test]
    fn from_profile_builds_amp_controller() {
        let section = ControllerSection {
            kind: ControllerKind::Amp,
            instance: Some("ATM10".into()),
            command: None,
        };
        let ctrl = AmpController::from_profile(&section).unwrap();
        assert_eq!(ctrl.ampinstmgr, "ampinstmgr");
        assert_eq!(ctrl.instance, "ATM10");
        assert_eq!(ctrl.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn from_profile_honors_configured_timeout() {
        let section = ControllerSection {
            kind: ControllerKind::Amp,
            instance: Some("ATM10".into()),
            command: Some(crate::config::profile::CommandConfig {
                status: vec![],
                stop: vec![],
                start: vec![],
                timeout_ms: Some(5_000),
            }),
        };
        assert_eq!(
            AmpController::from_profile(&section).unwrap().timeout_ms,
            5_000
        );
    }

    #[test]
    fn from_profile_rejects_wrong_kind() {
        let section = ControllerSection {
            kind: ControllerKind::Command,
            instance: None,
            command: None,
        };
        let err = AmpController::from_profile(&section).unwrap_err();
        assert!(matches!(err, PackError::Controller(_)));
    }

    #[test]
    fn from_profile_requires_instance() {
        let section = ControllerSection {
            kind: ControllerKind::Amp,
            instance: None,
            command: None,
        };
        let err = AmpController::from_profile(&section).unwrap_err();
        assert!(matches!(err, PackError::Controller(_)));
    }

    #[tokio::test]
    async fn status_parses_fake_ampinstmgr() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let binary = fake_ampinstmgr(dir.path(), &state);
        let ctrl = AmpController::with_binary(binary, "ATM10".into(), DEFAULT_TIMEOUT_MS);

        assert_eq!(ctrl.status().await.unwrap(), ServerStatus::Stopped);

        fs::write(&state, "running").unwrap();
        assert_eq!(ctrl.status().await.unwrap(), ServerStatus::Running);
    }

    #[tokio::test]
    async fn stop_confirms_stopped_and_start_confirms_running() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::write(&state, "running").unwrap();
        let binary = fake_ampinstmgr(dir.path(), &state);
        let ctrl = AmpController::with_binary(binary, "ATM10".into(), 5_000);

        assert_eq!(ctrl.status().await.unwrap(), ServerStatus::Running);
        ctrl.stop().await.unwrap();
        assert_eq!(ctrl.status().await.unwrap(), ServerStatus::Stopped);

        ctrl.start().await.unwrap();
        assert_eq!(ctrl.status().await.unwrap(), ServerStatus::Running);
    }

    #[tokio::test]
    async fn status_nonzero_exit_is_unknown_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let binary = write_executable(
            dir.path(),
            "ampinstmgr",
            "case \"$1\" in\nstatus) echo nope >&2; exit 1 ;;\n*) exit 1 ;;\nesac",
        );
        let ctrl = AmpController::with_binary(binary, "ATM10".into(), 1_000);
        assert_eq!(ctrl.status().await.unwrap(), ServerStatus::Unknown);
    }

    #[tokio::test]
    async fn status_missing_binary_errors() {
        let ctrl =
            AmpController::with_binary("/nonexistent/ampinstmgr".into(), "ATM10".into(), 5_000);
        let err = ctrl.status().await.unwrap_err();
        assert!(matches!(err, PackError::Controller(_)));
    }

    #[tokio::test]
    async fn stop_failure_errors_with_output() {
        let dir = tempfile::tempdir().unwrap();
        let binary = write_executable(
            dir.path(),
            "ampinstmgr",
            "echo \"cannot stop: something broke\" >&2; exit 1",
        );
        let ctrl = AmpController::with_binary(binary, "ATM10".into(), 5_000);
        let err = ctrl.stop().await.unwrap_err();
        match err {
            PackError::Controller(message) => {
                assert!(message.contains("cannot stop"), "message: {message}");
            }
            other => panic!("expected Controller error, got {other:?}"),
        }
    }
}
