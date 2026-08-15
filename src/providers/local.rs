//! Local archive pack provider.
//!
//! Prepares an upstream server pack from a local zip archive (or a directory
//! of zip archives) instead of a remote provider, so an update needs no
//! network access or API credentials. Each archive is one version, named by
//! its file stem.

use std::fs;
use std::path::{Path, PathBuf};

use tokio::task::JoinError;

use crate::error::{PackError, Result};
use crate::fs::extract::extract_server_pack;
use crate::providers::{
    PackProvider, PackRef, PackVersion, PreparedPack, ResolvedPackVersion, VersionSelector,
    resolve_version_from_list,
};

/// Local archive-backed pack provider.
pub struct LocalArchiveProvider {
    pub archive: PathBuf,
}

impl LocalArchiveProvider {
    pub fn new(archive: PathBuf) -> Self {
        Self { archive }
    }

    /// Every zip archive the provider exposes, newest first.
    ///
    /// A single archive file yields exactly one version; a directory yields
    /// each `*.zip` inside it, ordered by modification time descending (so the
    /// most recently added archive is "latest") with file name as tiebreak.
    fn archive_paths(&self) -> Result<Vec<PathBuf>> {
        let metadata = fs::metadata(&self.archive).map_err(|err| {
            let hint = if err.kind() == std::io::ErrorKind::NotFound {
                "; create the folder and drop a server-pack zip into it"
            } else {
                ""
            };
            PackError::Provider(format!(
                "archive '{}' is not accessible: {err}{hint}",
                self.archive.display()
            ))
        })?;

        if metadata.is_file() {
            return Ok(vec![self.archive.clone()]);
        }
        if !metadata.is_dir() {
            return Err(PackError::Provider(format!(
                "archive '{}' is neither a file nor a directory",
                self.archive.display()
            )));
        }

        let mut archives: Vec<(PathBuf, std::time::SystemTime)> = fs::read_dir(&self.archive)
            .map_err(|err| {
                PackError::io(
                    format!("list archives in '{}'", self.archive.display()),
                    err,
                )
            })?
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("zip") {
                    return None;
                }
                let modified = fs::metadata(&path).ok()?.modified().ok()?;
                Some((path, modified))
            })
            .collect();

        archives.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
        Ok(archives.into_iter().map(|(path, _)| path).collect())
    }

    /// The archive file for a resolved version, found by its file name.
    fn archive_for(&self, versions: &[PathBuf], version: &PackVersion) -> Result<PathBuf> {
        versions
            .iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == version.id)
            })
            .cloned()
            .ok_or_else(|| {
                PackError::Provider(format!(
                    "resolved local version '{}' no longer exists under '{}'",
                    version.name,
                    self.archive.display()
                ))
            })
    }
}

/// The modification time of `path` as an RFC-3339 string, when readable.
fn modified_rfc3339(path: &Path) -> Option<String> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339())
}

fn file_stem(file_name: &str) -> &str {
    match file_name.rfind('.') {
        Some(index) => &file_name[..index],
        None => file_name,
    }
}

#[async_trait::async_trait]
impl PackProvider for LocalArchiveProvider {
    async fn list_versions(&self, _pack: &PackRef) -> Result<Vec<PackVersion>> {
        let archives = self.archive_paths()?;
        Ok(archives
            .iter()
            .map(|path| {
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                PackVersion {
                    name: file_stem(&file_name).to_string(),
                    id: file_name,
                    file_id: None,
                    released: modified_rfc3339(path),
                }
            })
            .collect())
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
                .map(|v| v.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            PackError::Provider(format!(
                "no version matching {selector:?} for local archive '{}'; available versions: {}",
                self.archive.display(),
                available
            ))
        })?;
        Ok(ResolvedPackVersion {
            pack: pack.clone(),
            version: version.clone(),
        })
    }

    async fn prepare(&self, version: &ResolvedPackVersion, staging: &Path) -> Result<PreparedPack> {
        let archives = self.archive_paths()?;
        let archive = self.archive_for(&archives, &version.version)?;

        let server_root = staging.join("server");
        let archive_copy = archive.clone();
        let root = server_root.clone();
        let files = tokio::task::spawn_blocking(move || extract_server_pack(&archive_copy, &root))
            .await
            .map_err(spawn_error)??;

        Ok(PreparedPack {
            name: version.version.name.clone(),
            version: version.version.clone(),
            root: server_root,
            files,
        })
    }
}

fn spawn_error(error: JoinError) -> PackError {
    PackError::Other(format!("preparation task failed: {error}"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use zip::write::SimpleFileOptions;

    use super::*;

    fn pack_ref() -> PackRef {
        PackRef {
            project_id: 0,
            slug: String::new(),
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

    #[tokio::test]
    async fn single_archive_file_is_one_version() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("FTB StoneBlock 4 1.19.1.zip");
        write_zip(&zip, &[("mods/a.jar", b"aaa")]);

        let provider = LocalArchiveProvider::new(zip);
        let versions = provider.list_versions(&pack_ref()).await.unwrap();

        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].name, "FTB StoneBlock 4 1.19.1");
        assert_eq!(versions[0].id, "FTB StoneBlock 4 1.19.1.zip");
        assert!(versions[0].released.is_some());
    }

    #[tokio::test]
    async fn directory_lists_archives_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("pack-1.0.zip");
        let b = dir.path().join("pack-2.0.zip");
        write_zip(&a, &[("mods/a.jar", b"a")]);
        std::thread::sleep(Duration::from_millis(50));
        write_zip(&b, &[("mods/b.jar", b"b")]);

        let provider = LocalArchiveProvider::new(dir.path().to_path_buf());
        let versions = provider.list_versions(&pack_ref()).await.unwrap();

        assert_eq!(
            versions.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["pack-2.0", "pack-1.0"]
        );
    }

    #[tokio::test]
    async fn resolve_supports_latest_id_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("pack-1.0.zip");
        let b = dir.path().join("pack-2.0.zip");
        write_zip(&a, &[("mods/a.jar", b"a")]);
        std::thread::sleep(Duration::from_millis(50));
        write_zip(&b, &[("mods/b.jar", b"b")]);

        let provider = LocalArchiveProvider::new(dir.path().to_path_buf());
        let pack = pack_ref();

        let latest = provider
            .resolve_version(&pack, &VersionSelector::Latest)
            .await
            .unwrap();
        assert_eq!(latest.version.name, "pack-2.0");

        let by_name = provider
            .resolve_version(&pack, &VersionSelector::Name("pack-1.0".to_string()))
            .await
            .unwrap();
        assert_eq!(by_name.version.id, "pack-1.0.zip");

        let by_id = provider
            .resolve_version(&pack, &VersionSelector::Id("pack-2.0.zip".to_string()))
            .await
            .unwrap();
        assert_eq!(by_id.version.name, "pack-2.0");

        let missing = provider
            .resolve_version(&pack, &VersionSelector::Name("nope".to_string()))
            .await;
        assert!(matches!(missing, Err(PackError::Provider(_))));
    }

    #[tokio::test]
    async fn prepare_extracts_selected_archive() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("pack-1.0.zip");
        write_zip(&zip, &[("mods/a.jar", b"aaa"), ("config/x.toml", b"conf")]);

        let provider = LocalArchiveProvider::new(zip);
        let pack = pack_ref();
        let resolved = provider
            .resolve_version(&pack, &VersionSelector::Latest)
            .await
            .unwrap();

        let staging = tempfile::tempdir().unwrap();
        let prepared = provider.prepare(&resolved, staging.path()).await.unwrap();

        assert_eq!(prepared.version.name, "pack-1.0");
        assert_eq!(prepared.files.len(), 2);
        assert_eq!(
            std::fs::read(prepared.root.join("mods/a.jar")).unwrap(),
            b"aaa"
        );
    }

    #[tokio::test]
    async fn missing_archive_is_an_error() {
        let provider = LocalArchiveProvider::new(PathBuf::from("/nonexistent/pack.zip"));
        let err = provider.list_versions(&pack_ref()).await.unwrap_err();
        assert!(matches!(err, PackError::Provider(_)));
    }
}
