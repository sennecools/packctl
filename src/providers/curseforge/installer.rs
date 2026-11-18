//! CurseForge `PackProvider` implementation.
//!
//! A client pack version is selectable only when its own CurseForge file
//! metadata names a paired server pack via `serverPackFileId`.

use std::path::Path;

use tokio::task::JoinError;

use crate::core::reserved::ReservedPaths;
use crate::error::{PackError, Result};
use crate::fs::extract::extract_server_pack;
use crate::providers::curseforge::client::{CfClient, sanitize};
use crate::providers::curseforge::models::CfFile;
use crate::providers::{
    PackProvider, PackRef, PackVersion, PreparedPack, ResolvedPackVersion, VersionSelector,
    resolve_version_from_list,
};

/// CurseForge-backed pack provider.
pub struct CurseForgeProvider {
    pub client: CfClient,
    /// Paths the server pack may not write; entries there are skipped.
    pub reserved: ReservedPaths,
}

impl CurseForgeProvider {
    pub fn new(client: CfClient) -> Self {
        Self {
            client,
            reserved: ReservedPaths::default_layout(),
        }
    }

    /// Keeps the server pack away from `reserved` (the profile's own set).
    pub fn with_reserved(mut self, reserved: ReservedPaths) -> Self {
        self.reserved = reserved;
        self
    }
}

/// The numeric CurseForge project id of `pack`.
fn cf_project_id(pack: &PackRef) -> Result<u32> {
    pack.project_id.trim().parse::<u32>().map_err(|_| {
        PackError::Config(format!(
            "CurseForge project id '{}' is not a number",
            pack.project_id
        ))
    })
}

#[async_trait::async_trait]
impl PackProvider for CurseForgeProvider {
    async fn list_versions(&self, pack: &PackRef) -> Result<Vec<PackVersion>> {
        let mut files = self.client.get_files(cf_project_id(pack)?).await?;
        files.retain(is_selectable_client_file);
        sort_files_newest_first(&mut files);
        Ok(files.iter().map(version_from_file).collect())
    }

    async fn resolve_version(
        &self,
        pack: &PackRef,
        selector: &VersionSelector,
    ) -> Result<ResolvedPackVersion> {
        let versions = self.list_versions(pack).await?;
        let label = format!("pack '{}' (project {})", pack.slug, pack.project_id);
        let version = resolve_version_from_list(&versions, selector, &label)?;
        Ok(ResolvedPackVersion {
            pack: pack.clone(),
            version: version.clone(),
        })
    }

    async fn prepare(&self, version: &ResolvedPackVersion, staging: &Path) -> Result<PreparedPack> {
        let project_id = cf_project_id(&version.pack)?;

        tokio::fs::create_dir_all(staging).await.map_err(|e| {
            PackError::io(
                format!("create staging directory '{}'", staging.display()),
                e,
            )
        })?;

        let client_file_id = version.version.id.parse::<u32>().map_err(|_| {
            PackError::Provider(format!(
                "resolved version '{}' has a non-numeric CurseForge file id '{}'",
                version.version.name, version.version.id
            ))
        })?;
        let client_file = self.client.get_file(project_id, client_file_id).await?;
        let server_file_id = client_file.server_pack_file_id.ok_or_else(|| {
            PackError::Provider(format!(
                "resolved version '{}' (file {client_file_id}) for project {project_id} has no paired server pack",
                version.version.name
            ))
        })?;

        let server_file = self.client.get_file(project_id, server_file_id).await?;
        let archive_path = staging.join("curseforge-server-pack.zip");
        self.client
            .download_file_to(&server_file, &version.pack.slug, &archive_path)
            .await?;

        let server_root = staging.join("server");
        let archive = archive_path.clone();
        let root = server_root.clone();
        let reserved = self.reserved.clone();
        let extracted =
            tokio::task::spawn_blocking(move || extract_server_pack(&archive, &root, &reserved))
                .await
                .map_err(spawn_error)??;

        Ok(PreparedPack {
            name: version.pack.slug.clone(),
            version: version.version.clone(),
            root: server_root,
            requirements: None,
            notices: extracted.notices(),
            files: extracted.files,
        })
    }
}

/// Derives a `PackVersion` from a client modpack file.
pub(crate) fn version_from_file(file: &CfFile) -> PackVersion {
    let display = file.display_name.trim();
    // Names come from the endpoint (a third-party proxy by default); keep
    // control characters out of the terminal.
    let name = sanitize(if display.is_empty() {
        file_stem(&file.file_name)
    } else {
        display
    });
    let released = if file.file_date.is_empty() {
        None
    } else {
        Some(file.file_date.clone())
    };
    PackVersion {
        id: file.id.to_string(),
        name,
        released,
        game_versions: minecraft_versions(&file.game_versions),
        ..Default::default()
    }
}

/// The Minecraft versions among a file's CurseForge `gameVersions` tags.
///
/// CurseForge mixes Minecraft versions with loader, environment and Java
/// tags ("NeoForge", "Server", "Client", "Java 21"), so only dotted numeric
/// versions such as "1.21.1" or "26.1" are kept, in order and deduplicated.
fn minecraft_versions(tags: &[String]) -> Vec<String> {
    let mut versions: Vec<String> = Vec::new();
    for tag in tags.iter().map(|tag| tag.trim()) {
        if is_minecraft_version(tag) && !versions.iter().any(|v| v == tag) {
            versions.push(tag.to_string());
        }
    }
    versions
}

/// Whether `tag` is a dotted numeric version with at least two parts.
fn is_minecraft_version(tag: &str) -> bool {
    let parts: Vec<&str> = tag.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn is_selectable_client_file(file: &CfFile) -> bool {
    !file.is_server_pack && file.server_pack_file_id.is_some()
}

/// Sorts files newest first: releases (1) before betas (2) before alphas (3),
/// and within a release type by ISO file date descending. ISO-8601 dates sort
/// lexicographically.
pub(crate) fn sort_files_newest_first(files: &mut [CfFile]) {
    files.sort_by(|a, b| {
        a.release_type
            .cmp(&b.release_type)
            .then_with(|| b.file_date.cmp(&a.file_date))
    });
}

fn spawn_error(error: JoinError) -> PackError {
    PackError::Other(format!("preparation task failed: {error}"))
}

fn file_stem(file_name: &str) -> &str {
    match file_name.rfind('.') {
        Some(index) => &file_name[..index],
        None => file_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{PackVersion, VersionSelector};

    fn make_file(id: u32, display: &str, file_name: &str, release_type: u8, date: &str) -> CfFile {
        CfFile {
            id,
            display_name: display.to_string(),
            file_name: file_name.to_string(),
            file_date: date.to_string(),
            release_type,
            ..CfFile::default()
        }
    }

    #[test]
    fn version_from_file_uses_display_name_or_file_stem() {
        let with_display = make_file(
            10,
            "ATM10 2.41",
            "ATM10-2.41.zip",
            1,
            "2025-06-01T00:00:00Z",
        );
        let version = version_from_file(&with_display);
        assert_eq!(version.id, "10");
        assert_eq!(version.name, "ATM10 2.41");
        assert_eq!(version.released.as_deref(), Some("2025-06-01T00:00:00Z"));

        let bare = CfFile {
            display_name: String::new(),
            ..make_file(11, "ignored", "ServerPack-1.0.0.zip", 1, "")
        };
        let version = version_from_file(&bare);
        assert_eq!(version.name, "ServerPack-1.0.0");
        assert_eq!(version.released, None);
    }

    #[test]
    fn version_from_file_keeps_only_minecraft_game_versions() {
        let file = CfFile {
            game_versions: [
                "NeoForge",
                "1.21.1",
                "Server",
                "Client",
                "Java 21",
                "1.21",
                "1.21.1",
                "1.21-Snapshot",
                "26.1",
                " ",
                "1",
                "1.",
                ".1",
                "1..2",
            ]
            .iter()
            .map(|tag| tag.to_string())
            .collect(),
            ..make_file(12, "Pack 1.0", "Pack-1.0.zip", 1, "2025-06-01T00:00:00Z")
        };
        let version = version_from_file(&file);
        assert_eq!(version.game_versions, vec!["1.21.1", "1.21", "26.1"]);
        assert!(version.loaders.is_empty());

        let untagged = make_file(13, "Pack 1.1", "Pack-1.1.zip", 1, "2025-06-01T00:00:00Z");
        assert!(version_from_file(&untagged).game_versions.is_empty());
    }

    #[test]
    fn cf_project_id_rejects_non_numeric() {
        let pack = |id: &str| PackRef {
            project_id: id.to_string(),
            slug: String::new(),
        };
        assert_eq!(cf_project_id(&pack("925200")).unwrap(), 925200);
        for bad in ["AABBCC", "", "-1", "99999999999"] {
            let err = cf_project_id(&pack(bad)).unwrap_err();
            assert!(
                matches!(&err, PackError::Config(m)
                    if *m == format!("CurseForge project id '{bad}' is not a number")),
                "{err:?}"
            );
        }
    }

    #[test]
    fn sort_files_newest_first_orders_by_release_then_date() {
        let mut files = vec![
            make_file(1, "old alpha", "a.zip", 3, "2024-01-01T00:00:00Z"),
            make_file(2, "new beta", "b.zip", 2, "2025-06-01T00:00:00Z"),
            make_file(3, "old release", "c.zip", 1, "2024-06-01T00:00:00Z"),
            make_file(4, "new release", "d.zip", 1, "2025-06-01T00:00:00Z"),
        ];
        sort_files_newest_first(&mut files);
        let ids: Vec<u32> = files.iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![4, 3, 2, 1]);
    }

    #[test]
    fn resolve_version_selectors_match() {
        let versions = vec![
            PackVersion {
                id: "4".to_string(),
                name: "ATM10 2.41".to_string(),
                ..Default::default()
            },
            PackVersion {
                id: "5".to_string(),
                name: "ATM10 2.42".to_string(),
                ..Default::default()
            },
        ];

        let label = "pack 'atm10' (project 925200)";
        let resolve = |selector: VersionSelector| {
            resolve_version_from_list(&versions, &selector, label).map(|v| v.id.clone())
        };

        assert_eq!(resolve(VersionSelector::Latest).unwrap(), "4");
        assert_eq!(resolve(VersionSelector::Id("5".to_string())).unwrap(), "5");
        assert_eq!(resolve(VersionSelector::Id("4".to_string())).unwrap(), "4");
        assert_eq!(
            resolve(VersionSelector::Name("atm10 2.42".to_string())).unwrap(),
            "5"
        );
        assert_eq!(
            resolve(VersionSelector::Query("2.42".to_string())).unwrap(),
            "5"
        );
        assert!(matches!(
            resolve(VersionSelector::Name("missing".to_string())),
            Err(PackError::NotFound(_))
        ));
        assert!(matches!(
            resolve(VersionSelector::Id("999".to_string())),
            Err(PackError::NotFound(_))
        ));
    }

    #[test]
    fn only_client_files_with_an_explicit_server_mapping_are_selectable() {
        let unpaired = make_file(1, "client", "ATM10-2.41.zip", 1, "");
        let paired = CfFile {
            server_pack_file_id: Some(99),
            ..make_file(2, "client", "ATM10-2.42.zip", 1, "")
        };
        let server = CfFile {
            is_server_pack: true,
            server_pack_file_id: Some(100),
            ..make_file(99, "server", "ServerPack-2.42.zip", 1, "")
        };

        assert!(!is_selectable_client_file(&unpaired));
        assert!(is_selectable_client_file(&paired));
        assert!(!is_selectable_client_file(&server));
    }
}
