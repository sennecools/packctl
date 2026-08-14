//! Pack providers.
//!
//! A provider resolves modpacks and versions and prepares a selected version
//! in staging. Core update logic must not know how any specific provider
//! works.

pub mod curseforge;

use std::path::PathBuf;

use crate::error::Result;

/// The upstream pack being followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRef {
    pub project_id: u32,
    pub slug: String,
}

/// A single available version of a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackVersion {
    /// Provider-internal version id. Never trust human-readable parsing.
    pub id: String,
    /// Human-readable version label, e.g. "4.12".
    pub name: String,
    /// Provider file id, if the provider exposes one.
    pub file_id: Option<u32>,
    /// Release date, if known.
    pub released: Option<String>,
}

/// How the user selected a version.
#[derive(Debug, Clone)]
pub enum VersionSelector {
    /// Use the latest available version.
    Latest,
    /// Match by provider-internal id.
    Id(String),
    /// Match by human-readable name.
    Name(String),
}

/// A version that has been resolved to a concrete provider file.
#[derive(Debug, Clone)]
pub struct ResolvedPackVersion {
    pub pack: PackRef,
    pub version: PackVersion,
}

/// A file known to belong to a prepared upstream pack.
#[derive(Debug, Clone)]
pub struct PreparedFile {
    /// Relative path inside the server root.
    pub rel_path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

/// A prepared (downloaded + extracted) upstream version in staging.
#[derive(Debug)]
pub struct PreparedPack {
    pub name: String,
    pub version: PackVersion,
    /// Root of the prepared server tree inside the staging directory.
    pub root: PathBuf,
    pub files: Vec<PreparedFile>,
}

/// Resolves and prepares modpacks.
#[async_trait::async_trait]
pub trait PackProvider {
    async fn list_versions(&self, pack: &PackRef) -> Result<Vec<PackVersion>>;
    async fn resolve_version(
        &self,
        pack: &PackRef,
        selector: &VersionSelector,
    ) -> Result<ResolvedPackVersion>;
    async fn prepare(
        &self,
        version: &ResolvedPackVersion,
        staging: &PathBuf,
    ) -> Result<PreparedPack>;
}
