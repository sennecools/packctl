//! Server controllers.
//!
//! A controller manages server lifecycle only. AMP-specific commands must
//! remain inside the AMP controller; core update code never invokes controller
//! commands directly.

pub mod amp;
pub mod command;

use std::io::ErrorKind;
use std::process::Output;
use std::time::Duration;

use crate::error::{PackError, Result};

/// Reported lifecycle state of a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Running,
    Stopped,
    Unknown,
}

/// Run `argv` directly (never through a shell) bounded by `timeout`.
///
/// Spawning is retried a few times when the program is transiently busy
/// (ETXTBSY, e.g. a script still being written or replaced by another
/// process), which is otherwise an intermittent hard failure under load.
/// Timeouts and persistent spawn failures are returned as errors.
pub(crate) async fn run_argv_bounded(
    argv: &[String],
    timeout: Duration,
    what: &str,
    spawn_hint: &str,
) -> Result<Output> {
    const MAX_ATTEMPTS: u32 = 5;
    const RETRY_BACKOFF: Duration = Duration::from_millis(50);

    let mut attempt = 0u32;
    loop {
        attempt += 1;

        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.kill_on_drop(true);

        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => return Ok(output),
            Ok(Err(err))
                if err.kind() == ErrorKind::ExecutableFileBusy && attempt < MAX_ATTEMPTS =>
            {
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
            Ok(Err(err)) => {
                return Err(PackError::Controller(format!(
                    "{what} command could not be run: {}: {err}\n{spawn_hint}",
                    argv.join(" ")
                )));
            }
            Err(_elapsed) => {
                return Err(PackError::Controller(format!(
                    "{what} command timed out after {} ms: {}",
                    timeout.as_millis(),
                    argv.join(" ")
                )));
            }
        }
    }
}

/// Manages the lifecycle of a Minecraft server.
#[async_trait::async_trait]
pub trait ServerController: Send + Sync {
    async fn status(&self) -> Result<ServerStatus>;
    async fn stop(&self) -> Result<()>;
    async fn start(&self) -> Result<()>;
}

/// Builds the controller described by a profile's `[controller]` section.
pub fn from_profile(
    controller: &crate::config::profile::ControllerSection,
) -> Result<Box<dyn ServerController>> {
    match controller.kind {
        crate::config::profile::ControllerKind::Amp => {
            Ok(Box::new(amp::AmpController::from_profile(controller)?))
        }
        crate::config::profile::ControllerKind::Command => Ok(Box::new(
            command::CommandController::from_profile(controller)?,
        )),
    }
}
