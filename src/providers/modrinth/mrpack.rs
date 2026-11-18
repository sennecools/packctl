//! Reading Modrinth `.mrpack` files.
//!
//! An `.mrpack` is a zip with a `modrinth.index.json` at its root that lists
//! files to download (with hashes, sizes and mirror URLs) plus `overrides/`,
//! `server-overrides/` and `client-overrides/` folders copied on top. Every
//! value in the index is untrusted: paths go through the same safety checks
//! as archive entries, download URLs must be https on an allowlisted host,
//! and every file must carry a SHA-1, a SHA-512 and a size.
//!
//! Format: <https://support.modrinth.com/en/articles/8802351-modrinth-modpack-format-mrpack>.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::error::{PackError, Result};
use crate::fs::extract::{
    MAX_ENTRY_UNCOMPRESSED_BYTES, MAX_TOTAL_UNCOMPRESSED_BYTES, is_control_path,
};
use crate::fs::paths::normalize_relative;
use crate::providers::{LoaderKind, LoaderRequirement, PackRequirements, sanitize};

/// The index file at the root of every `.mrpack`.
pub const INDEX_FILE: &str = "modrinth.index.json";

/// The parsed `modrinth.index.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrpackIndex {
    pub format_version: u32,
    pub game: String,
    pub version_id: String,
    pub name: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub files: Vec<MrpackFile>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
}

/// One file the index asks to download.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrpackFile {
    pub path: String,
    pub hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub env: Option<MrpackEnv>,
    #[serde(default)]
    pub downloads: Vec<String>,
    /// The exact size in bytes. packctl requires it (see [`plan_files`]).
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Where a file is needed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MrpackEnv {
    #[serde(default)]
    pub client: EnvSupport,
    #[serde(default)]
    pub server: EnvSupport,
}

/// Whether a side needs a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvSupport {
    #[default]
    Required,
    Optional,
    Unsupported,
    #[serde(other)]
    Unknown,
}

/// Whether files the pack marks optional on the server are installed
/// (`[pack] optional_files`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OptionalFiles {
    #[default]
    Include,
    Exclude,
}

impl OptionalFiles {
    /// True for the default, which profiles leave out.
    pub fn is_default(&self) -> bool {
        *self == OptionalFiles::default()
    }
}

/// A verified download the server needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDownload {
    /// Safe path relative to the server root.
    pub rel_path: PathBuf,
    /// Allowed mirror URLs, in index order.
    pub urls: Vec<String>,
    pub size: u64,
    pub sha1: String,
    pub sha512: String,
}

/// What [`plan_files`] decided for the index's files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilePlan {
    pub downloads: Vec<PlannedDownload>,
    /// Paths skipped because the server does not support them.
    pub client_only: Vec<String>,
    /// Optional server files that were included.
    pub optional_included: Vec<String>,
    /// Optional server files that were left out.
    pub optional_excluded: Vec<String>,
}

/// Hosts the first download URL of a file may point to, as the format
/// recommends. Redirects from them may go anywhere over https; integrity
/// comes from the mandatory hashes.
pub const ALLOWED_DOWNLOAD_HOSTS: &[&str] = &[
    "cdn.modrinth.com",
    "github.com",
    "raw.githubusercontent.com",
    "gitlab.com",
];

/// Modrinth's staging CDN, allowed only for packs from the staging API.
pub const STAGING_CDN_HOST: &str = "staging-cdn.modrinth.com";

/// Which download hosts a pack's files may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DownloadHosts {
    /// [`ALLOWED_DOWNLOAD_HOSTS`].
    #[default]
    Public,
    /// [`ALLOWED_DOWNLOAD_HOSTS`] plus [`STAGING_CDN_HOST`], for packs
    /// served by Modrinth's staging API.
    WithStagingCdn,
}

impl DownloadHosts {
    fn allows(self, host: &str) -> bool {
        ALLOWED_DOWNLOAD_HOSTS.contains(&host)
            || (self == DownloadHosts::WithStagingCdn && host == STAGING_CDN_HOST)
    }
}

/// Largest `modrinth.index.json` accepted.
pub const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
/// Most files one index may list.
pub const MAX_INDEX_FILES: usize = 5000;

/// Reads and checks `modrinth.index.json` from an `.mrpack` (blocking IO).
pub fn read_index(mrpack: &Path) -> Result<MrpackIndex> {
    let file = std::fs::File::open(mrpack)
        .map_err(|e| PackError::io(format!("open '{}'", mrpack.display()), e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| {
        PackError::Parse(format!("invalid zip archive '{}': {e}", mrpack.display()))
    })?;
    let not_a_pack = || {
        PackError::Provider(format!(
            "'{}' is not a Modrinth pack: {INDEX_FILE} missing",
            mrpack.display()
        ))
    };
    let mut entry = match archive.by_name(INDEX_FILE) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Err(not_a_pack()),
        Err(e) => {
            return Err(PackError::Parse(format!(
                "read {INDEX_FILE} from '{}': {e}",
                mrpack.display()
            )));
        }
    };
    if entry.is_dir() {
        return Err(not_a_pack());
    }
    if entry.size() > MAX_INDEX_BYTES {
        return Err(PackError::Provider(format!(
            "mrpack index: {INDEX_FILE} is larger than {MAX_INDEX_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::new();
    (&mut entry)
        .take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| PackError::io(format!("read {INDEX_FILE}"), e))?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return Err(PackError::Provider(format!(
            "mrpack index: {INDEX_FILE} is larger than {MAX_INDEX_BYTES} bytes"
        )));
    }
    parse_index(&bytes)
}

/// Parses and checks index JSON.
pub fn parse_index(bytes: &[u8]) -> Result<MrpackIndex> {
    let index: MrpackIndex = serde_json::from_slice(bytes)
        .map_err(|e| PackError::Parse(format!("mrpack index: {e}")))?;
    if index.format_version != 1 {
        return Err(PackError::Provider(format!(
            "mrpack index: unsupported formatVersion {} (expected 1)",
            index.format_version
        )));
    }
    if index.game != "minecraft" {
        return Err(PackError::Provider(format!(
            "mrpack index: game '{}' is not minecraft",
            sanitize(&index.game)
        )));
    }
    if index.files.len() > MAX_INDEX_FILES {
        return Err(PackError::Provider(format!(
            "mrpack index: too many files ({}; limit {MAX_INDEX_FILES})",
            index.files.len()
        )));
    }
    Ok(index)
}

/// True for a well-formed lowercase-or-uppercase hex digest of `len` chars.
fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Decides which index files the server gets, and checks each one.
///
/// Files the server does not support are skipped as client-only; optional
/// server files follow `optional`; everything else is included. Each
/// included file needs a safe, unique path, a 40-hex SHA-1, a 128-hex
/// SHA-512, a size, and at least one allowed download URL. Sizes are capped
/// like archive extraction.
pub fn plan_files(
    index: &MrpackIndex,
    optional: OptionalFiles,
    hosts: DownloadHosts,
) -> Result<FilePlan> {
    let mut plan = FilePlan::default();
    let mut seen = HashSet::new();
    let mut total: u64 = 0;
    for file in &index.files {
        let rel = validate_index_path(&file.path)?;
        let shown = sanitize(&file.path);
        if !seen.insert(rel.to_string_lossy().to_ascii_lowercase()) {
            return Err(PackError::Provider(format!(
                "mrpack index: duplicate path '{shown}'"
            )));
        }
        let server = file
            .env
            .as_ref()
            .map_or(EnvSupport::Required, |env| env.server);
        match server {
            EnvSupport::Unsupported => {
                plan.client_only.push(shown);
                continue;
            }
            EnvSupport::Optional if optional == OptionalFiles::Exclude => {
                plan.optional_excluded.push(shown);
                continue;
            }
            EnvSupport::Optional => plan.optional_included.push(shown.clone()),
            EnvSupport::Required | EnvSupport::Unknown => {}
        }

        let hash = |algo: &str, len: usize| -> Result<String> {
            file.hashes
                .get(algo)
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| is_hex(value, len))
                .ok_or_else(|| {
                    PackError::Provider(format!(
                        "mrpack index: '{shown}' is missing a valid {algo} hash"
                    ))
                })
        };
        let sha1 = hash("sha1", 40)?;
        let sha512 = hash("sha512", 128)?;

        let size = file.file_size.ok_or_else(|| {
            PackError::Provider(format!("mrpack index: '{shown}' has no fileSize"))
        })?;
        total = total.saturating_add(size);
        if size > MAX_ENTRY_UNCOMPRESSED_BYTES || total > MAX_TOTAL_UNCOMPRESSED_BYTES {
            return Err(PackError::Provider(
                "mrpack index: pack exceeds size limits".to_string(),
            ));
        }

        let urls: Vec<String> = file
            .downloads
            .iter()
            .map(|url| url.trim())
            .filter(|url| validate_download_url(url, hosts).is_ok())
            .map(str::to_string)
            .collect();
        if urls.is_empty() {
            let first = file
                .downloads
                .first()
                .map(|url| sanitize(url))
                .unwrap_or_else(|| "none listed".to_string());
            return Err(PackError::Provider(format!(
                "mrpack index: '{shown}' has no allowed download URL ({first})"
            )));
        }

        plan.downloads.push(PlannedDownload {
            rel_path: rel,
            urls,
            size,
            sha1,
            sha512,
        });
    }
    Ok(plan)
}

/// The Minecraft and loader versions an index declares, plus notices.
///
/// `minecraft` maps to the Minecraft version; `neoforge`, `forge`,
/// `fabric-loader` and `quilt-loader` map to the loader, preferring that
/// order when several appear. Unknown keys are ignored.
pub fn requirements(index: &MrpackIndex) -> (PackRequirements, Vec<String>) {
    const LOADERS: &[(&str, LoaderKind)] = &[
        ("neoforge", LoaderKind::NeoForge),
        ("forge", LoaderKind::Forge),
        ("fabric-loader", LoaderKind::Fabric),
        ("quilt-loader", LoaderKind::Quilt),
    ];
    let value = |key: &str| {
        index
            .dependencies
            .get(key)
            .map(|v| sanitize(v.trim()))
            .filter(|v| !v.is_empty())
    };
    let declared: Vec<(&str, LoaderKind, String)> = LOADERS
        .iter()
        .filter_map(|(key, kind)| value(key).map(|version| (*key, *kind, version)))
        .collect();
    let mut notices = Vec::new();
    if declared.len() > 1 {
        let list = declared
            .iter()
            .map(|(key, _, _)| *key)
            .collect::<Vec<_>>()
            .join(", ");
        notices.push(format!(
            "Pack declares several loaders ({list}); using {}",
            declared[0].0
        ));
    }
    let requirements = PackRequirements {
        minecraft: value("minecraft"),
        loader: declared
            .into_iter()
            .next()
            .map(|(_, kind, version)| LoaderRequirement { kind, version }),
    };
    (requirements, notices)
}

/// Checks that a download URL is https on an allowed host.
pub fn validate_download_url(url: &str, hosts: DownloadHosts) -> Result<()> {
    let refuse = |reason: &str| {
        PackError::Provider(format!(
            "download URL '{}' is not allowed: {reason}",
            sanitize(url)
        ))
    };
    let parsed = Url::parse(url.trim()).map_err(|_| refuse("not a valid URL"))?;
    if parsed.scheme() != "https" {
        return Err(refuse("https is required"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(refuse("credentials in the URL are not supported"));
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if !hosts.allows(&host) {
        return Err(refuse(&format!("host '{host}' is not on the allowlist")));
    }
    Ok(())
}

/// Checks an index path and returns it normalized.
///
/// Backslashes, NUL bytes, absolute paths, drive letters, `.`/`..`/empty
/// components, and packctl's own control files (`.packctl/`,
/// `.packctl.toml`, `.packctlignore`) are refused. The profile-specific
/// reserved locations (overlay, archive) are not known here;
/// `build_from_mrpack` skips those with a notice.
pub fn validate_index_path(path: &str) -> Result<PathBuf> {
    let unsafe_path =
        || PackError::Provider(format!("mrpack index: unsafe path '{}'", sanitize(path)));
    let bytes = path.as_bytes();
    let drive_letter = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if path.contains(['\\', '\0']) || path.starts_with('/') || drive_letter {
        return Err(unsafe_path());
    }
    let rel = normalize_relative(Path::new(path)).map_err(|_| unsafe_path())?;
    if is_control_path(&rel) {
        return Err(unsafe_path());
    }
    Ok(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::modrinth::models::fixtures;

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";

    fn sha512() -> String {
        "ab".repeat(64)
    }

    fn entry(path: &str, server: Option<EnvSupport>) -> MrpackFile {
        MrpackFile {
            path: path.to_string(),
            hashes: BTreeMap::from([
                ("sha1".to_string(), SHA1.to_string()),
                ("sha512".to_string(), sha512()),
            ]),
            env: server.map(|server| MrpackEnv {
                client: EnvSupport::Required,
                server,
            }),
            downloads: vec![format!("https://cdn.modrinth.com/data/x/{path}")],
            file_size: Some(10),
        }
    }

    fn index(files: Vec<MrpackFile>) -> MrpackIndex {
        MrpackIndex {
            format_version: 1,
            game: "minecraft".to_string(),
            version_id: "1.0".to_string(),
            name: "Test".to_string(),
            summary: None,
            files,
            dependencies: BTreeMap::new(),
        }
    }

    fn provider_message(err: PackError) -> String {
        match err {
            PackError::Provider(message) => message,
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn index_fixture_parses_with_unknown_dependency_keys() {
        let index = parse_index(fixtures::INDEX.as_bytes()).unwrap();
        assert_eq!(index.name, "Cobblemon Modpack [Fabric]");
        assert_eq!(index.files.len(), 4);
        assert_eq!(
            index
                .dependencies
                .get("some-future-key")
                .map(String::as_str),
            Some("1.0")
        );
        let (requirements, notices) = requirements(&index);
        assert_eq!(requirements.minecraft.as_deref(), Some("1.21.1"));
        assert_eq!(
            requirements.loader,
            Some(LoaderRequirement {
                kind: LoaderKind::Fabric,
                version: "0.16.10".to_string()
            })
        );
        assert!(notices.is_empty(), "{notices:?}");

        let plan = plan_files(&index, OptionalFiles::Include, DownloadHosts::Public).unwrap();
        assert_eq!(plan.downloads.len(), 3);
        assert_eq!(
            plan.client_only,
            vec!["mods/sodium-fabric-0.6.5+mc1.21.1.jar"]
        );
        assert_eq!(
            plan.optional_included,
            vec!["mods/spark-1.10.109-fabric.jar"]
        );
        // github.com is an allowed first host.
        assert!(plan.downloads[2].urls[0].starts_with("https://github.com/"));
    }

    #[test]
    fn missing_env_is_required() {
        let plan = plan_files(
            &index(vec![entry("mods/a.jar", None)]),
            OptionalFiles::Exclude,
            DownloadHosts::Public,
        )
        .unwrap();
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].rel_path, PathBuf::from("mods/a.jar"));
        assert_eq!(plan.downloads[0].sha1, SHA1);
        assert_eq!(plan.downloads[0].size, 10);

        // An unknown env value counts as required too.
        let json = br#"{"formatVersion":1,"game":"minecraft","versionId":"1","name":"n",
            "files":[{"path":"mods/a.jar","hashes":{},"env":{"client":"required","server":"sometimes"}}]}"#;
        let parsed = parse_index(json).unwrap();
        assert_eq!(
            parsed.files[0].env.as_ref().unwrap().server,
            EnvSupport::Unknown
        );
    }

    #[test]
    fn server_unsupported_skipped_as_client_only() {
        let plan = plan_files(
            &index(vec![
                entry("mods/client.jar", Some(EnvSupport::Unsupported)),
                entry("mods/server.jar", Some(EnvSupport::Required)),
            ]),
            OptionalFiles::Include,
            DownloadHosts::Public,
        )
        .unwrap();
        assert_eq!(plan.client_only, vec!["mods/client.jar"]);
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].rel_path, PathBuf::from("mods/server.jar"));
    }

    #[test]
    fn optional_include_and_exclude() {
        let files = vec![
            entry("mods/opt.jar", Some(EnvSupport::Optional)),
            entry("mods/req.jar", Some(EnvSupport::Required)),
        ];
        let include = plan_files(
            &index(files.clone()),
            OptionalFiles::Include,
            DownloadHosts::Public,
        )
        .unwrap();
        assert_eq!(include.downloads.len(), 2);
        assert_eq!(include.optional_included, vec!["mods/opt.jar"]);
        assert!(include.optional_excluded.is_empty());

        let exclude =
            plan_files(&index(files), OptionalFiles::Exclude, DownloadHosts::Public).unwrap();
        assert_eq!(exclude.downloads.len(), 1);
        assert_eq!(exclude.optional_excluded, vec!["mods/opt.jar"]);
        assert!(exclude.optional_included.is_empty());
    }

    fn assert_unsafe(path: &str) {
        let message = provider_message(validate_index_path(path).unwrap_err());
        assert!(
            message.starts_with("mrpack index: unsafe path '"),
            "{path}: {message}"
        );
        // plan_files refuses it as well.
        assert!(
            plan_files(
                &index(vec![entry(path, None)]),
                OptionalFiles::Include,
                DownloadHosts::Public
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_dotdot() {
        assert_unsafe("../evil.jar");
        assert_unsafe("mods/../../evil.jar");
        assert_unsafe("mods/./a.jar");
    }

    #[test]
    fn rejects_drive_letter() {
        assert_unsafe("C:/Windows/evil.dll");
        assert_unsafe("c:evil.jar");
    }

    #[test]
    fn rejects_absolute() {
        assert_unsafe("/etc/passwd");
    }

    #[test]
    fn rejects_backslash() {
        assert_unsafe("mods\\a.jar");
        assert_unsafe("mods/a\0.jar");
    }

    #[test]
    fn rejects_packctl_dir() {
        for path in [
            ".packctl/state.json",
            ".PACKCTL/snapshots/x/manifest.json",
            ".packctl.toml",
            ".packctlignore",
        ] {
            assert_unsafe(path);
        }
        // Lookalikes deeper in the tree are fine.
        assert_eq!(
            validate_index_path("config/.packctl").unwrap(),
            PathBuf::from("config/.packctl")
        );
    }

    #[test]
    fn rejects_duplicate_paths() {
        let message = provider_message(
            plan_files(
                &index(vec![entry("mods/a.jar", None), entry("mods//a.jar", None)]),
                OptionalFiles::Include,
                DownloadHosts::Public,
            )
            .unwrap_err(),
        );
        // "mods//a.jar" is unsafe (empty component) before it is a duplicate.
        assert!(message.contains("unsafe path"), "{message}");

        let message = provider_message(
            plan_files(
                &index(vec![entry("mods/a.jar", None), entry("mods/A.jar", None)]),
                OptionalFiles::Include,
                DownloadHosts::Public,
            )
            .unwrap_err(),
        );
        assert_eq!(message, "mrpack index: duplicate path 'mods/A.jar'");
    }

    #[test]
    fn rejects_bad_hash_lengths() {
        let mut short = entry("mods/a.jar", None);
        short.hashes.insert("sha1".to_string(), "abc".to_string());
        let message = provider_message(
            plan_files(
                &index(vec![short]),
                OptionalFiles::Include,
                DownloadHosts::Public,
            )
            .unwrap_err(),
        );
        assert_eq!(
            message,
            "mrpack index: 'mods/a.jar' is missing a valid sha1 hash"
        );

        let mut missing = entry("mods/b.jar", None);
        missing.hashes.remove("sha512");
        let message = provider_message(
            plan_files(
                &index(vec![missing]),
                OptionalFiles::Include,
                DownloadHosts::Public,
            )
            .unwrap_err(),
        );
        assert_eq!(
            message,
            "mrpack index: 'mods/b.jar' is missing a valid sha512 hash"
        );

        let mut not_hex = entry("mods/c.jar", None);
        not_hex.hashes.insert("sha512".to_string(), "zz".repeat(64));
        assert!(
            plan_files(
                &index(vec![not_hex]),
                OptionalFiles::Include,
                DownloadHosts::Public
            )
            .is_err()
        );
    }

    #[test]
    fn missing_file_size() {
        let mut file = entry("mods/a.jar", None);
        file.file_size = None;
        let message = provider_message(
            plan_files(
                &index(vec![file]),
                OptionalFiles::Include,
                DownloadHosts::Public,
            )
            .unwrap_err(),
        );
        assert_eq!(message, "mrpack index: 'mods/a.jar' has no fileSize");
    }

    #[test]
    fn rejects_oversized_entries() {
        let mut file = entry("mods/a.jar", None);
        file.file_size = Some(MAX_ENTRY_UNCOMPRESSED_BYTES + 1);
        let message = provider_message(
            plan_files(
                &index(vec![file]),
                OptionalFiles::Include,
                DownloadHosts::Public,
            )
            .unwrap_err(),
        );
        assert_eq!(message, "mrpack index: pack exceeds size limits");
    }

    #[test]
    fn rejects_format_version_2() {
        let json =
            br#"{"formatVersion":2,"game":"minecraft","versionId":"1","name":"n","files":[]}"#;
        let message = provider_message(parse_index(json).unwrap_err());
        assert_eq!(
            message,
            "mrpack index: unsupported formatVersion 2 (expected 1)"
        );
    }

    #[test]
    fn rejects_non_minecraft_game() {
        let json =
            br#"{"formatVersion":1,"game":"terraria","versionId":"1","name":"n","files":[]}"#;
        let message = provider_message(parse_index(json).unwrap_err());
        assert_eq!(message, "mrpack index: game 'terraria' is not minecraft");
    }

    #[test]
    fn rejects_too_many_files() {
        let files = (0..=MAX_INDEX_FILES)
            .map(|i| format!(r#"{{"path":"mods/{i}.jar","hashes":{{}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"formatVersion":1,"game":"minecraft","versionId":"1","name":"n","files":[{files}]}}"#
        );
        let message = provider_message(parse_index(json.as_bytes()).unwrap_err());
        assert_eq!(
            message,
            format!("mrpack index: too many files (5001; limit {MAX_INDEX_FILES})")
        );
    }

    #[test]
    fn download_url_allowlist() {
        for ok in [
            "https://cdn.modrinth.com/data/AANobbMI/versions/x/sodium.jar",
            "https://github.com/FabricMC/fabric/releases/download/x/fabric-api.jar",
            "https://raw.githubusercontent.com/a/b/main/c.jar",
            "https://gitlab.com/a/b/-/raw/main/c.jar",
            "HTTPS://CDN.MODRINTH.COM/data/x.jar",
        ] {
            validate_download_url(ok, DownloadHosts::Public)
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "http://cdn.modrinth.com/data/x.jar",
            "https://evil.com/x.jar",
            "https://cdn.modrinth.com.evil.com/x.jar",
            "https://user@cdn.modrinth.com/x.jar",
            "ftp://cdn.modrinth.com/x.jar",
            "not a url",
        ] {
            assert!(
                validate_download_url(bad, DownloadHosts::Public).is_err(),
                "{bad}"
            );
        }

        // The staging CDN only for packs from the staging API.
        let staging = "https://staging-cdn.modrinth.com/data/x/versions/y/a.jar";
        assert!(validate_download_url(staging, DownloadHosts::Public).is_err());
        validate_download_url(staging, DownloadHosts::WithStagingCdn).unwrap();
        assert!(
            validate_download_url("https://evil.com/a.jar", DownloadHosts::WithStagingCdn).is_err()
        );

        // A file whose only URL is not allowed cannot be planned.
        let mut file = entry("mods/a.jar", None);
        file.downloads = vec!["https://evil.com/a.jar".to_string()];
        let message = provider_message(
            plan_files(
                &index(vec![file]),
                OptionalFiles::Include,
                DownloadHosts::Public,
            )
            .unwrap_err(),
        );
        assert_eq!(
            message,
            "mrpack index: 'mods/a.jar' has no allowed download URL (https://evil.com/a.jar)"
        );

        // Disallowed mirrors are dropped when an allowed one exists.
        let mut file = entry("mods/a.jar", None);
        file.downloads
            .insert(0, "http://mirror.example/a.jar".to_string());
        let plan = plan_files(
            &index(vec![file]),
            OptionalFiles::Include,
            DownloadHosts::Public,
        )
        .unwrap();
        assert_eq!(plan.downloads[0].urls.len(), 1);
    }

    #[test]
    fn requirements_maps_loaders_and_notes_multiple() {
        let mut idx = index(Vec::new());
        idx.dependencies = BTreeMap::from([
            ("minecraft".to_string(), "1.21.1".to_string()),
            ("fabric-loader".to_string(), "0.16.10".to_string()),
            ("neoforge".to_string(), "21.1.77".to_string()),
            ("unknown".to_string(), "1".to_string()),
        ]);
        let (requirements, notices) = requirements(&idx);
        assert_eq!(requirements.minecraft.as_deref(), Some("1.21.1"));
        assert_eq!(
            requirements.loader,
            Some(LoaderRequirement {
                kind: LoaderKind::NeoForge,
                version: "21.1.77".to_string()
            })
        );
        assert_eq!(
            notices,
            vec!["Pack declares several loaders (neoforge, fabric-loader); using neoforge"]
        );

        for (key, kind) in [
            ("forge", LoaderKind::Forge),
            ("quilt-loader", LoaderKind::Quilt),
        ] {
            let mut idx = index(Vec::new());
            idx.dependencies = BTreeMap::from([(key.to_string(), "1.0".to_string())]);
            let (requirements, notices) = super::requirements(&idx);
            assert_eq!(requirements.loader.unwrap().kind, kind);
            assert_eq!(requirements.minecraft, None);
            assert!(notices.is_empty());
        }
    }

    #[test]
    fn read_index_reports_missing_index() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-pack.mrpack");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        writer
            .start_file("overrides/a.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"a").unwrap();
        writer.finish().unwrap();
        let message = provider_message(read_index(&path).unwrap_err());
        assert_eq!(
            message,
            format!(
                "'{}' is not a Modrinth pack: modrinth.index.json missing",
                path.display()
            )
        );
    }
}
