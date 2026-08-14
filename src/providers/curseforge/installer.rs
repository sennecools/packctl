//! CurseForge `PackProvider` implementation.
//!
//! V1 design decision: version resolution operates on the client modpack files
//! for a pack, but the download used for an update is the mod's server pack
//! (`serverPackFileId`). When `serverPackFileId` is absent we fall back to
//! searching the file list for a file whose `fileName` contains "serverpack"
//! (case-insensitive). If no server pack can be found, preparation fails with
//! an actionable `PackError::Provider`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use tokio::task::JoinError;

use crate::error::{PackError, Result};
use crate::fs::hashing::sha256_file;
use crate::fs::paths::{normalize_relative, safe_join};
use crate::providers::curseforge::client::CfClient;
use crate::providers::curseforge::models::{CfFile, CfMod};
use crate::providers::{
    PackProvider, PackRef, PackVersion, PreparedFile, PreparedPack, ResolvedPackVersion,
    VersionSelector,
};

/// CurseForge-backed pack provider.
pub struct CurseForgeProvider {
    pub client: CfClient,
}

impl CurseForgeProvider {
    pub fn new(client: CfClient) -> Self {
        Self { client }
    }

    pub fn with_env() -> Result<Self> {
        Ok(Self::new(CfClient::from_env()?))
    }
}

#[async_trait::async_trait]
impl PackProvider for CurseForgeProvider {
    async fn list_versions(&self, pack: &PackRef) -> Result<Vec<PackVersion>> {
        let mut files = self.client.get_files(pack.project_id).await?;
        files.retain(|file| !file.is_server_pack_name());
        sort_files_newest_first(&mut files);
        Ok(files.iter().map(version_from_file).collect())
    }

    async fn resolve_version(
        &self,
        pack: &PackRef,
        selector: &VersionSelector,
    ) -> Result<ResolvedPackVersion> {
        let versions = self.list_versions(pack).await?;
        let version = resolve_version_from_list(&versions, selector).ok_or_else(|| {
            let available = versions
                .iter()
                .map(|v| format!("{} (id {})", v.name, v.id))
                .collect::<Vec<_>>()
                .join(", ");
            PackError::Provider(format!(
                "no version matching {selector:?} for pack '{}' (project {}); available versions: {}",
                pack.slug, pack.project_id, available
            ))
        })?;
        Ok(ResolvedPackVersion {
            pack: pack.clone(),
            version: version.clone(),
        })
    }

    async fn prepare(&self, version: &ResolvedPackVersion, staging: &Path) -> Result<PreparedPack> {
        let project_id = version.pack.project_id;

        tokio::fs::create_dir_all(staging).await.map_err(|e| {
            PackError::io(
                format!("create staging directory '{}'", staging.display()),
                e,
            )
        })?;

        let mod_info = self.client.get_mod(project_id).await?;
        let files = self.client.get_files(project_id).await?;

        let server_file_id = pick_server_pack(&mod_info, &files).ok_or_else(|| {
            PackError::Provider(format!(
                "cannot find a server pack for '{}' (project {project_id}): the mod has no \
                 serverPackFileId and none of its files is named like a server pack",
                mod_info.name
            ))
        })?;

        let server_file = self.client.get_file(project_id, server_file_id).await?;
        let download_url = server_file.download_url.clone().ok_or_else(|| {
            PackError::Provider(format!(
                "server pack file {server_file_id} of '{}' has no download URL",
                mod_info.name
            ))
        })?;

        let slug = if mod_info.slug.is_empty() {
            version.pack.slug.clone()
        } else {
            mod_info.slug.clone()
        };
        let archive_path = staging.join(format!("{slug}-server-pack.zip"));
        self.client
            .download_to(&download_url, &archive_path)
            .await?;

        let server_root = staging.join("server");
        let archive = archive_path.clone();
        let root = server_root.clone();
        let files = tokio::task::spawn_blocking(move || extract_server_pack(&archive, &root))
            .await
            .map_err(spawn_error)??;

        Ok(PreparedPack {
            name: mod_info.name,
            version: version.version.clone(),
            root: server_root,
            files,
        })
    }
}

/// Derives a `PackVersion` from a client modpack file.
pub(crate) fn version_from_file(file: &CfFile) -> PackVersion {
    let display = file.display_name.trim();
    let name = if display.is_empty() {
        file_stem(&file.file_name).to_string()
    } else {
        display.to_string()
    };
    let released = if file.file_date.is_empty() {
        None
    } else {
        Some(file.file_date.clone())
    };
    PackVersion {
        id: file.id.to_string(),
        name,
        file_id: Some(file.id),
        released,
    }
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

/// Resolves a version selector against a newest-first version list.
pub(crate) fn resolve_version_from_list<'a>(
    versions: &'a [PackVersion],
    selector: &VersionSelector,
) -> Option<&'a PackVersion> {
    match selector {
        VersionSelector::Latest => versions.first(),
        VersionSelector::Id(id) => versions
            .iter()
            .find(|v| v.id == *id || v.file_id.is_some_and(|file_id| file_id.to_string() == *id)),
        VersionSelector::Name(name) => {
            let needle = name.to_lowercase();
            versions.iter().find(|v| v.name.to_lowercase() == needle)
        }
    }
}

/// Picks the server pack file id for a mod.
///
/// Prefers the mod's `serverPackFileId`; otherwise falls back to the first file
/// whose name looks like a server pack.
pub(crate) fn pick_server_pack(mod_info: &CfMod, files: &[CfFile]) -> Option<u32> {
    if let Some(file_id) = mod_info.server_pack_file_id {
        return Some(file_id);
    }
    files
        .iter()
        .find(|file| file.is_server_pack_name())
        .map(|file| file.id)
}

/// Extracts a server pack zip into `dest_root`, enforcing strict path safety.
///
/// Absolute paths, `..` components, `.`/empty components, NUL bytes, backslash
/// traversal, and symlink entries abort the whole extraction with a
/// `PackError`; unsafe entries are never silently skipped. Directory entries
/// are skipped. Returns a `PreparedFile` for every extracted file.
///
/// This is CPU/IO-heavy synchronous code and should be called inside
/// `spawn_blocking` by async callers.
pub(crate) fn extract_server_pack(zip_path: &Path, dest_root: &Path) -> Result<Vec<PreparedFile>> {
    std::fs::create_dir_all(dest_root)
        .map_err(|e| PackError::io(format!("create '{}'", dest_root.display()), e))?;

    let archive_file = std::fs::File::open(zip_path)
        .map_err(|e| PackError::io(format!("open '{}'", zip_path.display()), e))?;
    let mut archive = zip::ZipArchive::new(archive_file).map_err(|e| {
        PackError::Parse(format!("invalid zip archive '{}': {e}", zip_path.display()))
    })?;

    let mut dest_paths = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| {
            PackError::Parse(format!(
                "read entry {index} from '{}': {e}",
                zip_path.display()
            ))
        })?;

        if is_symlink_entry(&entry) {
            return Err(PackError::UnsafePath(PathBuf::from(entry.name())));
        }
        if entry.is_dir() {
            continue;
        }

        let name = entry.name().to_string();
        let rel = normalize_relative(Path::new(&name))?;
        let dest = safe_join(dest_root, &rel)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PackError::io(format!("create '{}'", parent.display()), e))?;
        }

        let mut out = std::fs::File::create(&dest)
            .map_err(|e| PackError::io(format!("create '{}'", dest.display()), e))?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = entry
                .read(&mut buffer)
                .map_err(|e| PackError::io(format!("read zip entry '{name}'"), e))?;
            if read == 0 {
                break;
            }
            out.write_all(&buffer[..read])
                .map_err(|e| PackError::io(format!("write '{}'", dest.display()), e))?;
        }
        dest_paths.push(dest);
    }

    let mut prepared = Vec::with_capacity(dest_paths.len());
    for dest in dest_paths {
        let sha256 = sha256_file(&dest)?;
        let size = std::fs::metadata(&dest)
            .map_err(|e| PackError::io(format!("stat '{}'", dest.display()), e))?
            .len();
        let rel = dest.strip_prefix(dest_root).map_err(|_| {
            PackError::Other(format!(
                "extracted path escaped extraction root: '{}'",
                dest.display()
            ))
        })?;
        prepared.push(PreparedFile {
            rel_path: rel.to_path_buf(),
            size,
            sha256,
        });
    }
    Ok(prepared)
}

const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

fn is_symlink_entry(entry: &zip::read::ZipFile<'_>) -> bool {
    entry
        .unix_mode()
        .is_some_and(|mode| mode & S_IFMT == S_IFLNK)
}

fn file_stem(file_name: &str) -> &str {
    match file_name.rfind('.') {
        Some(index) => &file_name[..index],
        None => file_name,
    }
}

fn spawn_error(error: JoinError) -> PackError {
    PackError::Other(format!("preparation task failed: {error}"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::fs::hashing::sha256_bytes;
    use crate::providers::{PackVersion, VersionSelector};

    fn make_file(id: u32, display: &str, file_name: &str, release_type: u8, date: &str) -> CfFile {
        CfFile {
            id,
            display_name: display.to_string(),
            file_name: file_name.to_string(),
            file_date: date.to_string(),
            file_length: 0,
            download_url: None,
            release_type,
            file_hashes: Vec::new(),
            game_versions: Vec::new(),
        }
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, data) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn extract_server_pack_extracts_normal_zip() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.zip");
        write_zip(
            &zip_path,
            &[
                ("mods/a.jar", b"aaa"),
                ("mods/b.jar", b"bbbb"),
                ("config/example.toml", b"hello"),
                ("scripts/empty.txt", b""),
                ("sub\\inner.txt", b"nested"),
            ],
        );

        let dest = dir.path().join("server");
        let files = extract_server_pack(&zip_path, &dest).unwrap();

        let mut rels: Vec<PathBuf> = files.iter().map(|f| f.rel_path.clone()).collect();
        rels.sort();
        assert_eq!(
            rels,
            vec![
                PathBuf::from("config/example.toml"),
                PathBuf::from("mods/a.jar"),
                PathBuf::from("mods/b.jar"),
                PathBuf::from("scripts/empty.txt"),
                PathBuf::from("sub/inner.txt"),
            ]
        );

        let a = files
            .iter()
            .find(|f| f.rel_path == Path::new("mods/a.jar"))
            .unwrap();
        assert_eq!(a.size, 3);
        assert_eq!(a.sha256, sha256_bytes(b"aaa"));

        let empty = files
            .iter()
            .find(|f| f.rel_path == Path::new("scripts/empty.txt"))
            .unwrap();
        assert_eq!(empty.size, 0);

        assert_eq!(
            std::fs::read_to_string(dest.join("config/example.toml")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("sub/inner.txt")).unwrap(),
            "nested"
        );
        assert!(dest.join("mods/a.jar").exists());
    }

    #[test]
    fn extract_server_pack_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("evil.zip");
        write_zip(&zip_path, &[("mods/a.jar", b"aaa"), ("../evil", b"boom")]);

        let dest = dir.path().join("server");
        let result = extract_server_pack(&zip_path, &dest);
        assert!(matches!(result, Err(PackError::UnsafePathComponent { .. })));
        assert!(!dir.path().join("evil").exists());
        assert!(!dest.join("evil").exists());
    }

    #[test]
    fn extract_server_pack_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("abs.zip");
        write_zip(&zip_path, &[("/etc/evil", b"boom")]);

        let dest = dir.path().join("server");
        let result = extract_server_pack(&zip_path, &dest);
        assert!(matches!(result, Err(PackError::UnsafePath(_))));
    }

    #[test]
    fn extract_server_pack_rejects_backslash_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("bs.zip");
        write_zip(&zip_path, &[("..\\evil", b"boom")]);

        let dest = dir.path().join("server");
        let result = extract_server_pack(&zip_path, &dest);
        assert!(matches!(result, Err(PackError::UnsafePathComponent { .. })));
    }

    #[test]
    fn extract_server_pack_rejects_symlink_entry() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("symlink.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .add_symlink("mods/link.jar", "../real.jar", SimpleFileOptions::default())
                .unwrap();
            writer.finish().unwrap();
        }

        let dest = dir.path().join("server");
        let result = extract_server_pack(&zip_path, &dest);
        assert!(matches!(result, Err(PackError::UnsafePath(_))));
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
        assert_eq!(version.file_id, Some(10));
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
                file_id: Some(4),
                released: None,
            },
            PackVersion {
                id: "5".to_string(),
                name: "ATM10 2.42".to_string(),
                file_id: Some(5),
                released: None,
            },
        ];

        assert_eq!(
            resolve_version_from_list(&versions, &VersionSelector::Latest)
                .unwrap()
                .id,
            "4"
        );
        assert_eq!(
            resolve_version_from_list(&versions, &VersionSelector::Id("5".to_string()))
                .unwrap()
                .id,
            "5"
        );
        assert_eq!(
            resolve_version_from_list(&versions, &VersionSelector::Id("4".to_string()))
                .unwrap()
                .id,
            "4"
        );
        assert_eq!(
            resolve_version_from_list(&versions, &VersionSelector::Name("atm10 2.42".to_string()))
                .unwrap()
                .id,
            "5"
        );
        assert!(
            resolve_version_from_list(&versions, &VersionSelector::Name("missing".to_string()))
                .is_none()
        );
        assert!(
            resolve_version_from_list(&versions, &VersionSelector::Id("999".to_string())).is_none()
        );
    }

    #[test]
    fn pick_server_pack_prefers_server_pack_file_id() {
        let mod_info = CfMod {
            id: 925200,
            name: "ATM10".to_string(),
            slug: "all-the-mods-10".to_string(),
            server_pack_file_id: Some(99),
        };
        let files = vec![make_file(1, "client", "ATM10-2.41.zip", 1, "")];
        assert_eq!(pick_server_pack(&mod_info, &files), Some(99));
    }

    #[test]
    fn pick_server_pack_falls_back_to_named_file() {
        let mod_info = CfMod {
            id: 925200,
            name: "ATM10".to_string(),
            slug: "all-the-mods-10".to_string(),
            server_pack_file_id: None,
        };
        let files = vec![
            make_file(1, "client", "ATM10-2.41.zip", 1, ""),
            make_file(2, "server pack", "ServerPack-2.41.zip", 1, ""),
        ];
        assert_eq!(pick_server_pack(&mod_info, &files), Some(2));
    }

    #[test]
    fn pick_server_pack_none_when_no_server_pack() {
        let mod_info = CfMod {
            id: 925200,
            name: "ATM10".to_string(),
            slug: "all-the-mods-10".to_string(),
            server_pack_file_id: None,
        };
        let files = vec![make_file(1, "client", "ATM10-2.41.zip", 1, "")];
        assert_eq!(pick_server_pack(&mod_info, &files), None);
    }
}
