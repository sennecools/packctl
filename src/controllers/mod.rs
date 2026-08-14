//! Server controllers.
//!
//! A controller manages server lifecycle only. AMP-specific commands must
//! remain inside the AMP controller; core update code never invokes controller
//! commands directly.

pub mod amp;
pub mod command;

use crate::error::Result;

/// Reported lifecycle state of a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Running,
    Stopped,
    Unknown,
}

/// Manages the lifecycle of a Minecraft server.
#[async_trait::async_trait]
pub trait ServerController: Send + Sync {
    async fn status(&self) -> Result<ServerStatus>;
    async fn stop(&self) -> Result<()>;
    async fn start(&self) -> Result<()>;
}
