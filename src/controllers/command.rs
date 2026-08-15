//! Generic command-based server controller.
//!
//! The profile's `[controller.command]` section supplies argv arrays for the
//! `status`, `stop`, and `start` lifecycle commands. Commands are executed
//! directly (never through a shell) and each invocation is bounded by an
//! optional timeout.

use std::process::Output;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::config::profile::{ControllerKind, ControllerSection};
use crate::controllers::{ServerController, ServerStatus};
use crate::error::{PackError, Result};

/// Default per-command timeout and default lifecycle confirmation window.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Poll interval while waiting for a lifecycle transition.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A generic command-based server controller.
#[derive(Debug)]
pub struct CommandController {
    pub status: Vec<String>,
    pub stop: Vec<String>,
    pub start: Vec<String>,
    pub timeout_ms: Option<u64>,
}

impl CommandController {
    pub fn new(
        status: Vec<String>,
        stop: Vec<String>,
        start: Vec<String>,
        timeout_ms: Option<u64>,
    ) -> Self {
        CommandController {
            status,
            stop,
            start,
            timeout_ms,
        }
    }

    /// Build a controller from a profile `[controller]` section.
    ///
    /// Requires the `command` controller kind with a `[controller.command]`
    /// section. An `amp` section passed here is a configuration error.
    pub fn from_profile(controller: &ControllerSection) -> Result<Self> {
        match controller.kind {
            ControllerKind::Amp => Err(PackError::Controller(
                "cannot build a command controller from an 'amp' controller section".into(),
            )),
            ControllerKind::Command => {
                let Some(command) = &controller.command else {
                    return Err(PackError::Controller(
                        "command controller requires a [controller.command] section in the profile"
                            .into(),
                    ));
                };
                Ok(Self::new(
                    command.status.clone(),
                    command.stop.clone(),
                    command.start.clone(),
                    command.timeout_ms,
                ))
            }
        }
    }

    /// Run `argv` without a shell, bounded by the controller timeout.
    ///
    /// Returns the captured output for any exit code. Spawn failures (missing
    /// binary, permissions) and timeouts are errors; the caller maps the exit
    /// code.
    async fn run_command(&self, argv: &[String], what: &str) -> Result<Output> {
        if argv.is_empty() {
            return Err(PackError::Controller(format!(
                "{what} command is empty; check the profile's [controller.command] section"
            )));
        }

        let timeout = Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

        crate::controllers::run_argv_bounded(
            argv,
            timeout,
            what,
            "ensure the binary exists and is executable (check the profile's \
             [controller.command] section)",
        )
        .await
    }

    /// Wait for `status()` to report `target`, polling every ~1s up to the
    /// configured timeout (default [`DEFAULT_TIMEOUT_MS`]). [`ServerStatus::Unknown`]
    /// keeps polling.
    async fn wait_until(&self, target: ServerStatus) -> Result<()> {
        let timeout_ms = self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            if Instant::now() >= deadline {
                return Err(PackError::Controller(format!(
                    "server did not reach {target:?} within {timeout_ms} ms \
                     (status command: '{}')",
                    self.status.join(" ")
                )));
            }
            match self.status().await? {
                current if current == target => return Ok(()),
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }
}

/// Build a [`PackError::Controller`] for a command that exited non-zero,
/// including the exit code and stderr (when present) for context.
fn nonzero_exit_error(argv: &[String], what: &str, output: &Output) -> PackError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if stderr.trim().is_empty() {
        String::new()
    } else {
        format!("\nstderr: {}", stderr.trim())
    };
    PackError::Controller(format!(
        "{what} command exited with {:?}: {}{}",
        output.status.code(),
        argv.join(" "),
        detail
    ))
}

#[async_trait]
impl ServerController for CommandController {
    /// Map exit codes: 0 → [`ServerStatus::Running`], 1 → [`ServerStatus::Stopped`],
    /// anything else (including signal termination) → [`ServerStatus::Unknown`].
    /// A spawn failure is an error, never reported as stopped.
    async fn status(&self) -> Result<ServerStatus> {
        let output = self.run_command(&self.status, "status").await?;
        match output.status.code() {
            Some(0) => Ok(ServerStatus::Running),
            Some(1) => Ok(ServerStatus::Stopped),
            _ => Ok(ServerStatus::Unknown),
        }
    }

    async fn stop(&self) -> Result<()> {
        let output = self.run_command(&self.stop, "stop").await?;
        if !output.status.success() {
            return Err(nonzero_exit_error(&self.stop, "stop", &output));
        }
        self.wait_until(ServerStatus::Stopped).await
    }

    async fn start(&self) -> Result<()> {
        let output = self.run_command(&self.start, "start").await?;
        if !output.status.success() {
            return Err(nonzero_exit_error(&self.start, "start", &output));
        }
        self.wait_until(ServerStatus::Running).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profile::CommandConfig;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// Write `body` to an executable script inside `dir` and return its path.
    ///
    /// The script is written to a temporary sibling file and renamed into
    /// place so it is never executed while still open for writing (which would
    /// intermittently fail with ETXTBSY).
    fn write_script(dir: &Path, name: &str, body: &str) -> String {
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

    fn true_command() -> Vec<String> {
        vec!["/bin/true".into()]
    }

    #[tokio::test]
    async fn status_maps_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        for (code, expected) in [
            (0, ServerStatus::Running),
            (1, ServerStatus::Stopped),
            (2, ServerStatus::Unknown),
            (3, ServerStatus::Unknown),
        ] {
            let script = write_script(
                dir.path(),
                &format!("status_{code}.sh"),
                &format!("exit {code}"),
            );
            let ctrl = CommandController::new(vec![script], true_command(), true_command(), None);
            assert_eq!(ctrl.status().await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn status_missing_binary_errors() {
        let ctrl = CommandController::new(
            vec!["/nonexistent/packctl-test-binary".into()],
            true_command(),
            true_command(),
            None,
        );
        let err = ctrl.status().await.unwrap_err();
        match err {
            PackError::Controller(message) => {
                assert!(
                    message.contains("status"),
                    "message should mention the command: {message}"
                );
                assert!(
                    message.contains("nonexistent"),
                    "message should mention the binary: {message}"
                );
            }
            other => panic!("expected Controller error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stop_waits_until_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("stopped.flag");
        let status = write_script(
            dir.path(),
            "status.sh",
            &format!("if [ -f {} ]; then exit 1; else exit 0; fi", flag.display()),
        );
        let stop = write_script(dir.path(), "stop.sh", &format!("touch {}", flag.display()));
        let ctrl = CommandController::new(vec![status], vec![stop], true_command(), None);

        ctrl.stop().await.unwrap();
        assert_eq!(ctrl.status().await.unwrap(), ServerStatus::Stopped);
    }

    #[tokio::test]
    async fn stop_times_out_when_server_stays_running() {
        let dir = tempfile::tempdir().unwrap();
        let status = write_script(dir.path(), "status.sh", "exit 0");
        let ctrl =
            CommandController::new(vec![status], true_command(), true_command(), Some(1_000));

        let err = ctrl.stop().await.unwrap_err();
        match err {
            PackError::Controller(message) => {
                assert!(
                    message.contains("did not reach Stopped"),
                    "message should explain the timeout: {message}"
                );
            }
            other => panic!("expected Controller error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failing_stop_command_errors() {
        let dir = tempfile::tempdir().unwrap();
        let stop = write_script(dir.path(), "stop.sh", "echo boom >&2; exit 2");
        let ctrl = CommandController::new(
            vec![write_script(dir.path(), "status.sh", "exit 0")],
            vec![stop],
            true_command(),
            None,
        );

        let err = ctrl.stop().await.unwrap_err();
        match err {
            PackError::Controller(message) => {
                assert!(
                    message.contains("boom"),
                    "message should include stderr: {message}"
                );
            }
            other => panic!("expected Controller error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_waits_until_running() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("started.flag");
        let status = write_script(
            dir.path(),
            "status.sh",
            &format!("if [ -f {} ]; then exit 0; else exit 1; fi", flag.display()),
        );
        let start = write_script(dir.path(), "start.sh", &format!("touch {}", flag.display()));
        let ctrl = CommandController::new(vec![status], true_command(), vec![start], None);

        ctrl.start().await.unwrap();
        assert_eq!(ctrl.status().await.unwrap(), ServerStatus::Running);
    }

    #[test]
    fn from_profile_requires_command_section() {
        let section = ControllerSection {
            kind: ControllerKind::Command,
            instance: None,
            command: None,
        };
        let err = CommandController::from_profile(&section).unwrap_err();
        assert!(matches!(err, PackError::Controller(_)));
    }

    #[test]
    fn from_profile_rejects_amp_section() {
        let section = ControllerSection {
            kind: ControllerKind::Amp,
            instance: Some("minecraft".into()),
            command: None,
        };
        let err = CommandController::from_profile(&section).unwrap_err();
        assert!(matches!(err, PackError::Controller(_)));
    }

    #[test]
    fn from_profile_builds_controller() {
        let command = CommandConfig {
            status: vec!["pgrep".into(), "-f".into(), "server.jar".into()],
            stop: vec![
                "screen".into(),
                "-S".into(),
                "mc".into(),
                "-X".into(),
                "stuff".into(),
            ],
            start: vec!["systemctl".into(), "start".into(), "mc".into()],
            timeout_ms: Some(30_000),
        };
        let section = ControllerSection {
            kind: ControllerKind::Command,
            instance: None,
            command: Some(command),
        };

        let ctrl = CommandController::from_profile(&section).unwrap();
        assert_eq!(ctrl.status, ["pgrep", "-f", "server.jar"]);
        assert_eq!(ctrl.start, ["systemctl", "start", "mc"]);
        assert_eq!(ctrl.timeout_ms, Some(30_000));
    }
}
