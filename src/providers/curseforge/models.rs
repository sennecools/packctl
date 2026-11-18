//! Serde models for the CurseForge API (v1).
//!
//! The shapes follow the official API (and the community proxy, which
//! mirrors it byte for byte): single resources are wrapped in a
//! `{"data": ...}` envelope, lists add `pagination`, and file hashes are
//! `hashes: [{value, algo}]` with 1 = SHA-1 and 2 = MD5.
//!
//! All structs tolerate unknown JSON fields (serde ignores them by default)
//! and default optional fields to empty values so partial payloads still
//! parse. JSON field names are camelCase; Rust field names are snake_case.

use serde::Deserialize;

/// CurseForge class id of modpacks (as opposed to mods, 6).
pub const CLASS_ID_MODPACKS: u32 = 4471;

/// A CurseForge project. A modpack is a project whose files are modpack
/// versions.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfMod {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub slug: String,
    /// Project class: [`CLASS_ID_MODPACKS`] for modpacks, 6 for mods.
    #[serde(default)]
    pub class_id: Option<u32>,
    /// `false` when the author disabled downloads through third-party tools;
    /// files then come back without a `downloadUrl`.
    #[serde(default)]
    pub allow_mod_distribution: Option<bool>,
    /// File id of the latest server pack, when the API reports one.
    #[serde(default)]
    pub server_pack_file_id: Option<u32>,
}

/// A file belonging to a project (a modpack version when the project is a
/// modpack).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfFile {
    pub id: u32,
    /// The project the file belongs to.
    #[serde(default)]
    pub mod_id: u32,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub file_name: String,
    /// ISO-8601 release date; lexicographic ordering matches chronological.
    #[serde(default)]
    pub file_date: String,
    #[serde(default)]
    pub file_length: u64,
    /// CDN URL of the file. `null` when the author disabled third-party
    /// distribution.
    #[serde(default)]
    pub download_url: Option<String>,
    /// 1 = Release, 2 = Beta, 3 = Alpha.
    #[serde(default)]
    pub release_type: u8,
    #[serde(default, rename = "hashes")]
    pub hashes: Vec<CfFileHash>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    /// Whether this file is itself a server pack rather than a client pack version.
    #[serde(default)]
    pub is_server_pack: bool,
    /// File id of the server pack paired with this client pack version.
    #[serde(default)]
    pub server_pack_file_id: Option<u32>,
    /// CurseForge's MurmurHash2 fingerprint of the file (kept for future
    /// version detection).
    #[serde(default)]
    pub file_fingerprint: Option<u64>,
}

impl CfFile {
    /// SHA-1 hash (algo 1), when the file reports one.
    pub fn sha1_hash(&self) -> Option<&str> {
        self.hash(1)
    }

    /// MD5 hash (algo 2), when the file reports one.
    pub fn md5_hash(&self) -> Option<&str> {
        self.hash(2)
    }

    fn hash(&self, algo: u8) -> Option<&str> {
        self.hashes
            .iter()
            .find(|hash| hash.algo == algo)
            .map(|hash| hash.value.as_str())
            .filter(|value| !value.trim().is_empty())
    }
}

/// A file hash reported by the API. `algo` is 1 = SHA-1, 2 = MD5.
#[derive(Debug, Clone, Deserialize)]
pub struct CfFileHash {
    pub value: String,
    pub algo: u8,
}

/// Envelope returned by `GET /v1/mods/{modId}`.
#[derive(Debug, Deserialize)]
pub struct CfModResponse {
    pub data: CfMod,
}

/// Envelope returned by `GET /v1/mods/{modId}/files/{fileId}`.
#[derive(Debug, Deserialize)]
pub struct CfFileResponse {
    pub data: CfFile,
}

/// Envelope returned by `GET /v1/mods/{modId}/files`.
#[derive(Debug, Deserialize)]
pub struct CfFileListResponse {
    pub data: Vec<CfFile>,
    #[serde(default)]
    pub pagination: CfPagination,
}

/// Envelope returned by `GET /v1/mods/search`.
#[derive(Debug, Deserialize)]
pub struct CfModSearchResponse {
    pub data: Vec<CfMod>,
    #[serde(default)]
    pub pagination: CfPagination,
}

/// Pagination metadata of a list page.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfPagination {
    #[serde(default)]
    pub index: u64,
    #[serde(default)]
    pub page_size: u64,
    #[serde(default)]
    pub result_count: u64,
    #[serde(default)]
    pub total_count: u64,
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Recorded-shape API responses (official shape, invented values).
    pub const MOD_925200: &str = include_str!("fixtures/mod_925200.json");
    pub const FILE_CLIENT: &str = include_str!("fixtures/file_client.json");
    pub const FILE_SERVER: &str = include_str!("fixtures/file_server.json");
    pub const FILES_PAGE: &str = include_str!("fixtures/files_page.json");
    pub const SEARCH: &str = include_str!("fixtures/search.json");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_response_envelope_parses_fixture() {
        let parsed: CfModResponse = serde_json::from_str(fixtures::MOD_925200).unwrap();
        let project = parsed.data;
        assert_eq!(project.id, 925200);
        assert_eq!(project.name, "All the Mods 10 - ATM10");
        assert_eq!(project.slug, "all-the-mods-10");
        assert_eq!(project.class_id, Some(CLASS_ID_MODPACKS));
        assert_eq!(project.allow_mod_distribution, Some(true));
    }

    #[test]
    fn file_response_envelope_parses_fixture() {
        let client: CfFileResponse = serde_json::from_str(fixtures::FILE_CLIENT).unwrap();
        let client = client.data;
        assert_eq!(client.id, 6900001);
        assert_eq!(client.mod_id, 925200);
        assert_eq!(client.display_name, "All the Mods 10-4.12");
        assert_eq!(client.file_name, "All the Mods 10-4.12.zip");
        assert_eq!(client.file_date, "2026-11-10T18:15:28.950Z");
        assert_eq!(client.file_length, 1_835_264);
        assert_eq!(client.release_type, 1);
        assert!(!client.is_server_pack);
        assert_eq!(client.server_pack_file_id, Some(6900002));
        assert_eq!(client.game_versions, vec!["NeoForge", "1.21.1"]);
        assert_eq!(client.file_fingerprint, Some(3_612_872_210));

        let server: CfFileResponse = serde_json::from_str(fixtures::FILE_SERVER).unwrap();
        let server = server.data;
        assert!(server.is_server_pack);
        assert_eq!(server.server_pack_file_id, None);
        assert_eq!(
            server.download_url.as_deref(),
            Some("https://edge.forgecdn.net/files/6900/2/ServerFiles-4.12.zip")
        );
        assert_eq!(server.sha1_hash().map(str::len), Some(40));
    }

    #[test]
    fn files_page_parses_pagination() {
        let parsed: CfFileListResponse = serde_json::from_str(fixtures::FILES_PAGE).unwrap();
        assert_eq!(parsed.data.len(), 3);
        assert_eq!(parsed.pagination.index, 50);
        assert_eq!(parsed.pagination.page_size, 50);
        assert_eq!(parsed.pagination.result_count, 3);
        assert_eq!(parsed.pagination.total_count, 53);
        // The author of the old beta disabled third-party distribution.
        assert_eq!(parsed.data[2].download_url, None);
        assert_eq!(parsed.data[2].release_type, 2);
    }

    #[test]
    fn search_response_lists_projects_of_every_class() {
        let parsed: CfModSearchResponse = serde_json::from_str(fixtures::SEARCH).unwrap();
        let classes: Vec<_> = parsed.data.iter().map(|m| (m.id, m.class_id)).collect();
        assert_eq!(
            classes,
            vec![(328085, Some(6)), (372026, Some(CLASS_ID_MODPACKS))]
        );
        assert_eq!(parsed.data[1].allow_mod_distribution, Some(false));
        assert_eq!(parsed.pagination.total_count, 2);
    }

    #[test]
    fn sha1_and_md5_from_real_shaped_hashes() {
        let parsed: CfFileResponse = serde_json::from_str(fixtures::FILE_CLIENT).unwrap();
        assert_eq!(
            parsed.data.sha1_hash(),
            Some("19b0a7e2a1b3c4d5e6f708192a3b4c5d6e7f8091")
        );
        assert_eq!(
            parsed.data.md5_hash(),
            Some("802f6a1b2c3d4e5f60718293a4b5c6d7")
        );
    }

    #[test]
    fn missing_sha1_returns_none() {
        let only_md5: CfFile = serde_json::from_str(
            r#"{"id": 1, "hashes": [{"value": "802f6a1b2c3d4e5f60718293a4b5c6d7", "algo": 2}]}"#,
        )
        .unwrap();
        assert_eq!(only_md5.sha1_hash(), None);
        assert!(only_md5.md5_hash().is_some());

        // The invented pre-fix shape (`fileHashes`/`algoId`) carries no hashes.
        let legacy: CfFile = serde_json::from_str(
            r#"{"id": 2, "fileHashes": [{"value": "19b0a7e2a1b3c4d5e6f708192a3b4c5d6e7f8091", "algoId": 2}]}"#,
        )
        .unwrap();
        assert_eq!(legacy.sha1_hash(), None);

        let blank: CfFile =
            serde_json::from_str(r#"{"id": 3, "hashes": [{"value": " ", "algo": 1}]}"#).unwrap();
        assert_eq!(blank.sha1_hash(), None);
    }

    #[test]
    fn cf_mod_defaults_absent_optional_fields() {
        let parsed: CfMod = serde_json::from_str(r#"{"id": 1, "name": "Minimal"}"#).unwrap();
        assert_eq!(parsed.id, 1);
        assert_eq!(parsed.name, "Minimal");
        assert_eq!(parsed.slug, "");
        assert_eq!(parsed.class_id, None);
        assert_eq!(parsed.allow_mod_distribution, None);
        assert_eq!(parsed.server_pack_file_id, None);
    }

    #[test]
    fn cf_file_defaults_empty_optionals() {
        let parsed: CfFile = serde_json::from_str(r#"{"id": 7}"#).unwrap();
        assert_eq!(parsed.id, 7);
        assert_eq!(parsed.mod_id, 0);
        assert_eq!(parsed.display_name, "");
        assert_eq!(parsed.file_name, "");
        assert_eq!(parsed.file_date, "");
        assert_eq!(parsed.file_length, 0);
        assert_eq!(parsed.download_url, None);
        assert_eq!(parsed.release_type, 0);
        assert!(parsed.hashes.is_empty());
        assert!(parsed.game_versions.is_empty());
        assert!(!parsed.is_server_pack);
        assert_eq!(parsed.server_pack_file_id, None);
        assert_eq!(parsed.file_fingerprint, None);
        assert_eq!(parsed.sha1_hash(), None);
    }

    #[test]
    fn file_list_response_defaults_missing_pagination() {
        let parsed: CfFileListResponse = serde_json::from_str(r#"{"data": []}"#).unwrap();
        assert!(parsed.data.is_empty());
        assert_eq!(parsed.pagination.total_count, 0);
        assert_eq!(parsed.pagination.index, 0);
        assert_eq!(parsed.pagination.page_size, 0);
    }
}
