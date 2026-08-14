//! Serde models for the CurseForge API (api.curseforge.com, v1).
//!
//! All structs tolerate unknown JSON fields (serde ignores them by default)
//! and default optional fields to empty values so partial payloads still
//! parse. JSON field names are camelCase; Rust field names are snake_case.

#![allow(dead_code)]

use serde::Deserialize;

/// A CurseForge mod. A modpack is a mod whose files are modpack versions.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfMod {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub slug: String,
    /// File id of the latest server pack, when the mod ships one.
    #[serde(default)]
    pub server_pack_file_id: Option<u32>,
}

/// A file belonging to a mod (a modpack version when the mod is a modpack).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfFile {
    pub id: u32,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub file_name: String,
    /// ISO-8601 release date; lexicographic ordering matches chronological.
    #[serde(default)]
    pub file_date: String,
    #[serde(default)]
    pub file_length: u64,
    #[serde(default)]
    pub download_url: Option<String>,
    /// 1 = Release, 2 = Beta, 3 = Alpha.
    #[serde(default)]
    pub release_type: u8,
    #[serde(default)]
    pub file_hashes: Vec<CfFileHash>,
    #[serde(default)]
    pub game_versions: Vec<String>,
}

impl CfFile {
    /// SHA-256 hash (algoId 3), when the file reports one.
    pub fn sha256_hash(&self) -> Option<&str> {
        self.file_hashes
            .iter()
            .find(|hash| hash.algo_id == 3)
            .map(|hash| hash.value.as_str())
    }

    /// True when the file name looks like a dedicated server pack.
    ///
    /// CurseForge server packs are typically named like
    /// `ServerPack-1.2.3.zip`; the check is case-insensitive.
    pub fn is_server_pack_name(&self) -> bool {
        self.file_name.to_lowercase().contains("serverpack")
    }
}

/// A file hash reported by the API. `algo_id` is 1 = MD5, 2 = SHA1, 3 = SHA256.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfFileHash {
    pub value: String,
    pub algo_id: u8,
}

/// Envelope returned by `GET /v1/mods/{modId}/files`.
#[derive(Debug, Deserialize)]
pub struct CfFileListResponse {
    pub data: Vec<CfFile>,
    #[serde(default)]
    pub pagination: CfPagination,
}

/// Pagination metadata for a file list page.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfPagination {
    #[serde(default)]
    pub total_count: u64,
    #[serde(default)]
    pub index: u64,
    #[serde(default)]
    pub page_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_mod_parses_full_fixture() {
        let json = r#"
        {
            "id": 925200,
            "gameId": 432,
            "name": "All the Mods 10",
            "slug": "all-the-mods-10",
            "summary": "A kitchen sink pack",
            "status": 4,
            "downloadCount": 123456,
            "serverPackFileId": 99999,
            "latestFiles": []
        }
        "#;
        let parsed: CfMod = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.id, 925200);
        assert_eq!(parsed.name, "All the Mods 10");
        assert_eq!(parsed.slug, "all-the-mods-10");
        assert_eq!(parsed.server_pack_file_id, Some(99999));
    }

    #[test]
    fn cf_mod_defaults_absent_optional_fields() {
        let parsed: CfMod = serde_json::from_str(r#"{"id": 1, "name": "Minimal"}"#).unwrap();
        assert_eq!(parsed.id, 1);
        assert_eq!(parsed.name, "Minimal");
        assert_eq!(parsed.slug, "");
        assert_eq!(parsed.server_pack_file_id, None);
    }

    #[test]
    fn cf_file_parses_full_fixture() {
        let json = r#"
        {
            "id": 12345,
            "modId": 925200,
            "gameId": 432,
            "displayName": "ATM10 2.41",
            "fileName": "ATM10-2.41.zip",
            "fileDate": "2025-06-01T12:00:00Z",
            "fileLength": 10485760,
            "releaseType": 1,
            "fileStatus": 4,
            "downloadUrl": "https://edge.forgecdn.net/files/1234/5678/ATM10-2.41.zip",
            "isAlternate": false,
            "gameVersions": ["1.21.1", "Fabric"],
            "fileHashes": [
                {"value": "d41d8cd98f00b204e9800998ecf8427e", "algoId": 1},
                {"value": "da39a3ee5e6b4b0d3255bfef95601890afd80709", "algoId": 2},
                {"value": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "algoId": 3}
            ]
        }
        "#;
        let parsed: CfFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.id, 12345);
        assert_eq!(parsed.display_name, "ATM10 2.41");
        assert_eq!(parsed.file_name, "ATM10-2.41.zip");
        assert_eq!(parsed.file_date, "2025-06-01T12:00:00Z");
        assert_eq!(parsed.file_length, 10_485_760);
        assert_eq!(parsed.release_type, 1);
        assert_eq!(
            parsed.download_url.as_deref(),
            Some("https://edge.forgecdn.net/files/1234/5678/ATM10-2.41.zip")
        );
        assert_eq!(parsed.game_versions, vec!["1.21.1", "Fabric"]);
        assert_eq!(
            parsed.sha256_hash(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert!(!parsed.is_server_pack_name());
    }

    #[test]
    fn cf_file_defaults_empty_optionals() {
        let parsed: CfFile = serde_json::from_str(r#"{"id": 7}"#).unwrap();
        assert_eq!(parsed.id, 7);
        assert_eq!(parsed.display_name, "");
        assert_eq!(parsed.file_name, "");
        assert_eq!(parsed.file_date, "");
        assert_eq!(parsed.file_length, 0);
        assert_eq!(parsed.download_url, None);
        assert_eq!(parsed.release_type, 0);
        assert!(parsed.file_hashes.is_empty());
        assert!(parsed.game_versions.is_empty());
        assert_eq!(parsed.sha256_hash(), None);
    }

    #[test]
    fn sha256_hash_prefers_algo_3() {
        let json = r#"
        {
            "id": 1,
            "fileHashes": [
                {"value": "md5-value", "algoId": 1},
                {"value": "sha256-value", "algoId": 3}
            ]
        }
        "#;
        let parsed: CfFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.sha256_hash(), Some("sha256-value"));
    }

    #[test]
    fn file_list_response_parses_with_pagination() {
        let json = r#"
        {
            "data": [
                {"id": 1, "displayName": "One", "fileName": "one.zip", "releaseType": 1},
                {"id": 2, "displayName": "Two", "fileName": "two.zip", "releaseType": 1}
            ],
            "pagination": {"index": 0, "pageSize": 50, "totalCount": 2}
        }
        "#;
        let parsed: CfFileListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].id, 1);
        assert_eq!(parsed.data[1].id, 2);
        assert_eq!(parsed.pagination.total_count, 2);
        assert_eq!(parsed.pagination.index, 0);
        assert_eq!(parsed.pagination.page_size, 50);
    }

    #[test]
    fn file_list_response_defaults_missing_pagination() {
        let parsed: CfFileListResponse = serde_json::from_str(r#"{"data": []}"#).unwrap();
        assert!(parsed.data.is_empty());
        assert_eq!(parsed.pagination.total_count, 0);
        assert_eq!(parsed.pagination.index, 0);
        assert_eq!(parsed.pagination.page_size, 0);
    }

    #[test]
    fn is_server_pack_name_detects_server_packs() {
        let server = CfFile {
            id: 1,
            display_name: String::new(),
            file_name: "ServerPack-2.41.zip".to_string(),
            file_date: String::new(),
            file_length: 0,
            download_url: None,
            release_type: 1,
            file_hashes: Vec::new(),
            game_versions: Vec::new(),
        };
        assert!(server.is_server_pack_name());

        let client = CfFile {
            id: 2,
            display_name: String::new(),
            file_name: "ATM10-2.41.zip".to_string(),
            file_date: String::new(),
            file_length: 0,
            download_url: None,
            release_type: 1,
            file_hashes: Vec::new(),
            game_versions: Vec::new(),
        };
        assert!(!client.is_server_pack_name());
    }
}
