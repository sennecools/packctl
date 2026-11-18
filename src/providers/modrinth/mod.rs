//! Modrinth modpack provider.
//!
//! Modrinth needs no API key. A pack version is an `.mrpack`: an index of
//! files to download plus override folders. packctl builds the server side
//! of it in staging (see [`provider`]) and reports, but does not install,
//! the mod loader it needs.

pub mod client;
pub mod models;
pub mod mrpack;
pub mod provider;
pub mod source;
