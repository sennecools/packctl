//! Modrinth API v2 response models.
//!
//! Only the fields packctl uses are modelled; unknown fields are ignored so
//! API additions never break parsing.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::providers::{PackVersion, sanitize};

/// A Modrinth project (`GET /project/{id|slug}`).
#[derive(Debug, Clone, Deserialize)]
pub struct MrProject {
    /// The base62 project id, e.g. `5FFgwNNP`.
    pub id: String,
    pub slug: String,
    pub title: String,
    /// `modpack`, `mod`, `resourcepack`, `shader`, `plugin`, `datapack`.
    pub project_type: String,
    /// `required`, `optional`, `unsupported` or `unknown`.
    #[serde(default)]
    pub server_side: Option<String>,
    #[serde(default)]
    pub client_side: Option<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
}

/// One version of a project (`GET /project/{id}/version`, `GET /version/{id}`).
#[derive(Debug, Clone, Deserialize)]
pub struct MrVersion {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub version_number: String,
    /// RFC 3339 publication time.
    pub date_published: String,
    /// `release`, `beta` or `alpha`.
    pub version_type: String,
    /// `listed`, `archived`, `draft`, `unlisted`, `scheduled` or `unknown`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub files: Vec<MrFile>,
    /// Other versions this one depends on. A modpack lists the files its
    /// `.mrpack` bundles as `embedded` dependencies.
    #[serde(default)]
    pub dependencies: Vec<MrDependency>,
}

/// A dependency of a version.
#[derive(Debug, Clone, Deserialize)]
pub struct MrDependency {
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    /// `required`, `optional`, `incompatible` or `embedded`.
    #[serde(default)]
    pub dependency_type: String,
}

impl MrVersion {
    /// The files this version embeds, as `(version id, project id)` pairs.
    /// The project id is `None` when Modrinth does not report it.
    pub fn embedded_versions(&self) -> Vec<(&str, Option<&str>)> {
        self.dependencies
            .iter()
            .filter(|dep| dep.dependency_type == "embedded")
            .filter_map(|dep| {
                dep.version_id
                    .as_deref()
                    .map(|version| (version, dep.project_id.as_deref()))
            })
            .collect()
    }
}

/// A file attached to a version.
#[derive(Debug, Clone, Deserialize)]
pub struct MrFile {
    #[serde(default)]
    pub hashes: MrHashes,
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub primary: bool,
    pub size: u64,
    #[serde(default)]
    pub file_type: Option<String>,
}

/// The hashes Modrinth reports for a file. Missing hashes stay empty and
/// fail verification, so a file without them is never installed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MrHashes {
    #[serde(default)]
    pub sha1: String,
    #[serde(default)]
    pub sha512: String,
}

fn is_mrpack(file: &MrFile) -> bool {
    file.filename.to_ascii_lowercase().ends_with(".mrpack")
}

/// The `.mrpack` file of a version: the primary file when it is one,
/// otherwise the first `.mrpack`.
pub fn pack_file(version: &MrVersion) -> Option<&MrFile> {
    version
        .files
        .iter()
        .find(|file| file.primary && is_mrpack(file))
        .or_else(|| version.files.iter().find(|file| is_mrpack(file)))
}

/// Releases sort before betas, betas before alphas, anything else last.
fn type_rank(version_type: &str) -> u8 {
    match version_type {
        "release" => 0,
        "beta" => 1,
        "alpha" => 2,
        _ => 3,
    }
}

/// The publication time, when it parses.
pub(crate) fn parse_date(date: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(date.trim())
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

/// Sorts versions newest first: releases before betas before alphas, then by
/// publication time descending (parsed, not compared as text). Versions
/// whose date does not parse sort last within their type.
pub fn sort_versions_newest_first(versions: &mut [MrVersion]) {
    versions.sort_by(|a, b| {
        type_rank(&a.version_type)
            .cmp(&type_rank(&b.version_type))
            .then_with(
                || match (parse_date(&a.date_published), parse_date(&b.date_published)) {
                    (Some(a), Some(b)) => b.cmp(&a),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                },
            )
    });
}

/// True for versions users may install: not a draft and not scheduled.
pub fn is_listed(version: &MrVersion) -> bool {
    !matches!(version.status.as_deref(), Some("draft" | "scheduled"))
}

/// The provider-neutral view of a version. Names are author-controlled, so
/// control characters are replaced before they can reach a terminal.
pub fn version_from_mr(version: &MrVersion) -> PackVersion {
    let name = sanitize(version.name.trim());
    let number = sanitize(version.version_number.trim());
    PackVersion {
        id: version.id.clone(),
        name: if name.is_empty() {
            number.clone()
        } else {
            name
        },
        number: (!number.is_empty()).then_some(number),
        released: Some(version.date_published.clone()),
        game_versions: version.game_versions.iter().map(|v| sanitize(v)).collect(),
        loaders: version.loaders.iter().map(|v| sanitize(v)).collect(),
    }
}

/// JSON fixtures trimmed from real API responses, for tests.
#[cfg(test)]
pub(crate) mod fixtures {
    pub const PROJECT: &str = include_str!("fixtures/project.json");
    pub const VERSIONS: &str = include_str!("fixtures/versions.json");
    pub const INDEX: &str = include_str!("fixtures/index.json");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(id: &str, version_type: &str, date: &str) -> MrVersion {
        MrVersion {
            id: id.to_string(),
            project_id: "p".to_string(),
            name: id.to_string(),
            version_number: id.to_string(),
            date_published: date.to_string(),
            version_type: version_type.to_string(),
            status: None,
            loaders: Vec::new(),
            game_versions: Vec::new(),
            files: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    fn file(name: &str, primary: bool) -> MrFile {
        MrFile {
            hashes: MrHashes::default(),
            url: format!("https://cdn.modrinth.com/data/x/{name}"),
            filename: name.to_string(),
            primary,
            size: 1,
            file_type: None,
        }
    }

    #[test]
    fn project_fixture_parses() {
        let project: MrProject = serde_json::from_str(fixtures::PROJECT).unwrap();
        assert_eq!(project.id, "5FFgwNNP");
        assert_eq!(project.slug, "cobblemon-modpack");
        assert_eq!(project.project_type, "modpack");
        assert_eq!(project.server_side.as_deref(), Some("required"));
        assert_eq!(project.loaders, vec!["fabric"]);
        assert!(!project.game_versions.is_empty());
    }

    #[test]
    fn versions_fixture_parses() {
        let versions: Vec<MrVersion> = serde_json::from_str(fixtures::VERSIONS).unwrap();
        assert_eq!(versions.len(), 4);
        let first = &versions[0];
        assert_eq!(first.id, "YQpPe5Iu");
        assert_eq!(first.version_type, "release");
        let pack = pack_file(first).unwrap();
        assert_eq!(pack.filename, "Cobblemon Modpack [Fabric] 1.6.1.mrpack");
        assert_eq!(pack.hashes.sha1.len(), 40);
        assert_eq!(pack.hashes.sha512.len(), 128);
        assert!(pack.primary);
    }

    #[test]
    fn pack_file_prefers_primary_mrpack() {
        let mut v = version("a", "release", "2024-01-01T00:00:00Z");
        v.files = vec![
            file("changelog.txt", true),
            file("first.mrpack", false),
            file("primary.MRPACK", true),
        ];
        assert_eq!(pack_file(&v).unwrap().filename, "primary.MRPACK");

        // A primary non-mrpack file falls back to the first .mrpack.
        v.files = vec![file("readme.md", true), file("pack.mrpack", false)];
        assert_eq!(pack_file(&v).unwrap().filename, "pack.mrpack");

        v.files = vec![file("server.zip", true)];
        assert!(pack_file(&v).is_none());
    }

    #[test]
    fn versions_without_mrpack_filtered() {
        let versions: Vec<MrVersion> = serde_json::from_str(fixtures::VERSIONS).unwrap();
        let usable: Vec<&str> = versions
            .iter()
            .filter(|v| is_listed(v) && pack_file(v).is_some())
            .map(|v| v.id.as_str())
            .collect();
        // One version only ships a zip; one is a draft.
        assert_eq!(usable, vec!["YQpPe5Iu", "Pz8kd2Lm"]);
    }

    #[test]
    fn sort_release_before_beta_then_date_desc() {
        let mut versions = vec![
            version("old-release", "release", "2024-01-01T00:00:00Z"),
            version("new-beta", "beta", "2024-06-01T00:00:00Z"),
            version("alpha", "alpha", "2024-07-01T00:00:00Z"),
            // As text this sorts after "2024-04-30..."; as a time it is
            // 2024-04-30T23:00Z, so the parsed dates must decide.
            version("offset-release", "release", "2024-05-01T01:00:00+02:00"),
            version("mid-release", "release", "2024-04-30T23:30:00Z"),
            version("undated", "release", "not a date"),
        ];
        sort_versions_newest_first(&mut versions);
        let order: Vec<&str> = versions.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "mid-release",
                "offset-release",
                "old-release",
                "undated",
                "new-beta",
                "alpha"
            ]
        );
    }

    #[test]
    fn drafts_filtered() {
        let mut v = version("a", "release", "2024-01-01T00:00:00Z");
        for (status, listed) in [
            (None, true),
            (Some("listed"), true),
            (Some("unlisted"), true),
            (Some("archived"), true),
            (Some("draft"), false),
            (Some("scheduled"), false),
        ] {
            v.status = status.map(str::to_string);
            assert_eq!(is_listed(&v), listed, "{status:?}");
        }
    }

    #[test]
    fn version_from_mr_maps_and_sanitizes() {
        let mut v = version("abc", "release", "2024-01-01T00:00:00Z");
        v.name = "Pack 1.0\n# injected".to_string();
        v.version_number = "1.0".to_string();
        v.game_versions = vec!["1.21.1".to_string()];
        v.loaders = vec!["neoforge".to_string()];
        let pv = version_from_mr(&v);
        assert_eq!(pv.id, "abc");
        assert_eq!(pv.name, "Pack 1.0 # injected");
        assert_eq!(pv.number.as_deref(), Some("1.0"));
        assert_eq!(pv.released.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(pv.game_versions, vec!["1.21.1"]);
        assert_eq!(pv.loaders, vec!["neoforge"]);
    }
}
