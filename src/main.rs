//! packctl — safe, modular CLI for updating self-hosted Minecraft modpack servers.
//!
//! The update engine treats a running server as three layers:
//!   UPSTREAM MODPACK + LOCAL OVERLAY + PERSISTENT RUNTIME DATA.
//! See design notes for the full model.

mod cli;
mod config;
mod controllers;
mod core;
mod error;
mod fs;
mod providers;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await?;
    Ok(())
}
