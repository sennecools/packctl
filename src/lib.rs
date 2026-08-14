//! packctl — safe, modular CLI for updating self-hosted Minecraft modpack servers.
//!
//! The update engine treats a running server as three layers:
//!   UPSTREAM MODPACK + LOCAL OVERLAY + PERSISTENT RUNTIME DATA.
//! See design notes for the full model.
//!
//! The core is host-local and callable programmatically so a future remote
//! client can invoke the same server-side operations without redesign.

pub mod cli;
pub mod config;
pub mod controllers;
pub mod core;
pub mod error;
pub mod fs;
pub mod providers;
