//! Local archive pack provider.
//!
//! Prepares an upstream server pack from a local server-pack zip or Modrinth
//! `.mrpack` (or a directory of them) instead of a remote provider, so an
//! update needs no API credentials. Each archive is one version, named by its
//! file stem. A zip is extracted as it is; an `.mrpack` goes through the
//! Modrinth pipeline, which downloads (and verifies) the files its index
//! lists unless an identical copy is already on the server.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::task::JoinError;

use crate::core::detect::{JarPrint, VersionFingerprint};
use crate::core::reserved::ReservedPaths;
use crate::error::{PackError, Result};
use crate::fs::extract::{common_wrapper, extract_server_pack};
use crate::providers::http::{Fetcher, HttpFetcher};
use crate::providers::modrinth::mrpack::{DownloadHosts, EnvSupport, OptionalFiles, read_index};
use crate::providers::modrinth::provider::{
    DEFAULT_DOWNLOAD_CONCURRENCY, MrpackContext, build_from_mrpack,
};
use crate::providers::{
    PackProvider, PackRef, PackVersion, PreparedPack, ResolvedPackVersion, VersionSelector,
    resolve_version_from_list,
};

/// Local archive-backed pack provider.
pub struct LocalArchiveProvider {
    pub archive: PathBuf,
    /// The live server root, whose identical files an `.mrpack` reuses.
    pub reuse_root: Option<PathBuf>,
    /// Whether optional server files of an `.mrpack` are installed.
    pub optional_files: OptionalFiles,
    /// Paths the pack may not write; files there are skipped.
    pub reserved: ReservedPaths,
    /// Downloads the files an `.mrpack` lists; an [`HttpFetcher`] when unset.
    fetcher: Option<Arc<dyn Fetcher>>,
}

/// True for the archive kinds the local provider reads: `.zip` and
/// `.mrpack`, in any letter case.
fn is_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip") || is_mrpack_ext(ext))
}

fn is_mrpack_ext(ext: &str) -> bool {
    ext.eq_ignore_ascii_case("mrpack")
}

fn is_mrpack(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(is_mrpack_ext)
}

impl LocalArchiveProvider {
    pub fn new(archive: PathBuf) -> Self {
        Self {
            archive,
            reuse_root: None,
            optional_files: OptionalFiles::default(),
            reserved: ReservedPaths::default_layout(),
            fetcher: None,
        }
    }

    /// Keeps the pack away from `reserved` (the profile's own set).
    pub fn with_reserved(mut self, reserved: ReservedPaths) -> Self {
        self.reserved = reserved;
        self
    }

    /// Reuses identical files from the live server at `root` when building
    /// an `.mrpack`.
    pub fn with_reuse_root(mut self, root: PathBuf) -> Self {
        self.reuse_root = Some(root);
        self
    }

    /// Sets whether optional server files of an `.mrpack` are installed.
    pub fn with_optional_files(mut self, optional_files: OptionalFiles) -> Self {
        self.optional_files = optional_files;
        self
    }

    /// Downloads `.mrpack` files through `fetcher` (tests use a fake).
    pub fn with_fetcher(mut self, fetcher: Arc<dyn Fetcher>) -> Self {
        self.fetcher = Some(fetcher);
        self
    }

    /// Every archive the provider exposes, newest first.
    ///
    /// A single archive file yields exactly one version; a directory yields
    /// each `*.zip` and `*.mrpack` inside it (any letter case), ordered by
    /// modification time descending (so the most recently added archive is
    /// "latest") with file name as tiebreak.
    fn archive_paths(&self) -> Result<Vec<PathBuf>> {
        let metadata = fs::metadata(&self.archive).map_err(|err| {
            let hint = if err.kind() == std::io::ErrorKind::NotFound {
                "; create the folder and drop a server-pack .zip or .mrpack into it"
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
                if !is_archive(&path) {
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
                    released: modified_rfc3339(path),
                    ..Default::default()
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
        let label = format!("local archive '{}'", self.archive.display());
        let version = resolve_version_from_list(&versions, selector, &label)?;
        Ok(ResolvedPackVersion {
            pack: pack.clone(),
            version: version.clone(),
        })
    }

    async fn prepare(&self, version: &ResolvedPackVersion, staging: &Path) -> Result<PreparedPack> {
        let archives = self.archive_paths()?;
        let archive = self.archive_for(&archives, &version.version)?;

        let server_root = staging.join("server");
        if is_mrpack(&archive) {
            return self
                .prepare_mrpack(&archive, &server_root, &version.version)
                .await;
        }
        let archive_copy = archive.clone();
        let root = server_root.clone();
        let reserved = self.reserved.clone();
        let extracted = tokio::task::spawn_blocking(move || {
            extract_server_pack(&archive_copy, &root, &reserved)
        })
        .await
        .map_err(spawn_error)??;

        Ok(PreparedPack {
            name: version.version.name.clone(),
            version: version.version.clone(),
            root: server_root,
            requirements: None,
            notices: extracted.notices(),
            files: extracted.files,
        })
    }

    async fn version_fingerprints(
        &self,
        _pack: &PackRef,
        versions: &[PackVersion],
    ) -> Result<Vec<VersionFingerprint>> {
        let archives = self.archive_paths()?;
        let mut fingerprints = Vec::with_capacity(versions.len());
        for version in versions {
            let archive = self.archive_for(&archives, version)?;
            let jars = tokio::task::spawn_blocking(move || archive_jar_prints(&archive))
                .await
                .map_err(spawn_error)??;
            fingerprints.push(VersionFingerprint {
                version: version.clone(),
                jars,
            });
        }
        Ok(fingerprints)
    }
}

/// The jars an archive puts into `mods/`, read without extracting anything.
///
/// A server-pack zip is read from its central directory (name, uncompressed
/// size, CRC-32), after the same wrapper-directory stripping extraction
/// does. An `.mrpack` contributes the `mods/` files its index lists for the
/// server (SHA-1 and size) plus the jars in its `overrides/` and
/// `server-overrides/` folders.
pub(crate) fn archive_jar_prints(archive: &Path) -> Result<Vec<JarPrint>> {
    let file = fs::File::open(archive)
        .map_err(|error| PackError::io(format!("open '{}'", archive.display()), error))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| {
        PackError::Provider(format!(
            "'{}' is not a readable archive: {error}",
            archive.display()
        ))
    })?;
    let mut entries: Vec<(PathBuf, u64, u32)> = Vec::with_capacity(zip.len());
    for index in 0..zip.len() {
        let entry = zip.by_index_raw(index).map_err(|error| {
            PackError::Provider(format!("cannot read '{}': {error}", archive.display()))
        })?;
        if entry.is_dir() {
            continue;
        }
        let name = crate::fs::extract::entry_name(&entry, archive)?.replace('\\', "/");
        entries.push((PathBuf::from(name), entry.size(), entry.crc32()));
    }

    let mut prints = Vec::new();
    let mrpack = is_mrpack(archive);
    let prefixes: &[&str] = if mrpack {
        &["overrides", "server-overrides"]
    } else {
        &[""]
    };
    let wrapper = if mrpack {
        None
    } else {
        let names: Vec<PathBuf> = entries.iter().map(|(name, _, _)| name.clone()).collect();
        common_wrapper(&names)
    };
    for (name, size, crc32) in &entries {
        let rel = match &wrapper {
            Some(wrapper) => name.strip_prefix(wrapper).unwrap_or(name),
            None => name.as_path(),
        };
        for prefix in prefixes {
            let Ok(inner) = rel.strip_prefix(prefix) else {
                continue;
            };
            if let Some(file_name) = mods_jar_name(inner) {
                prints.push(JarPrint {
                    file_name,
                    size: *size,
                    crc32: Some(*crc32),
                    sha1: None,
                });
            }
        }
    }
    if mrpack {
        let index = read_index(archive)?;
        for file in &index.files {
            let unsupported = file
                .env
                .as_ref()
                .is_some_and(|env| env.server == EnvSupport::Unsupported);
            let Some(file_name) = mods_jar_name(Path::new(&file.path)) else {
                continue;
            };
            if unsupported {
                continue;
            }
            prints.push(JarPrint {
                file_name,
                size: file.file_size.unwrap_or_default(),
                crc32: None,
                sha1: file
                    .hashes
                    .get("sha1")
                    .map(|sha1| sha1.to_ascii_lowercase()),
            });
        }
    }
    prints.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(prints)
}

/// The file name of `mods/<name>.jar` directly inside `mods/`.
fn mods_jar_name(rel: &Path) -> Option<String> {
    let mut components = rel.components();
    let first = components.next()?;
    let name = components.next()?;
    if components.next().is_some() || first.as_os_str() != "mods" {
        return None;
    }
    let name = name.as_os_str().to_str()?;
    crate::fs::paths::is_jar_path(Path::new(name)).then(|| name.to_string())
}

impl LocalArchiveProvider {
    /// Builds a local `.mrpack` with the Modrinth pipeline.
    async fn prepare_mrpack(
        &self,
        archive: &Path,
        server_root: &Path,
        version: &PackVersion,
    ) -> Result<PreparedPack> {
        let fetcher = self
            .fetcher
            .clone()
            .unwrap_or_else(|| Arc::new(HttpFetcher::new()));
        let ctx = MrpackContext {
            fetcher: fetcher.as_ref(),
            reuse_root: self.reuse_root.as_deref(),
            optional_files: self.optional_files,
            concurrency: DEFAULT_DOWNLOAD_CONCURRENCY,
            reserved: &self.reserved,
            download_hosts: DownloadHosts::Public,
        };
        let outcome = build_from_mrpack(archive, server_root, &ctx).await?;
        Ok(PreparedPack {
            name: version.name.clone(),
            version: version.clone(),
            root: server_root.to_path_buf(),
            files: outcome.files,
            requirements: Some(outcome.requirements),
            notices: outcome.notices,
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
    use crate::providers::modrinth::provider::fake::{FakeFetcher, write_mrpack};

    fn pack_ref() -> PackRef {
        PackRef {
            project_id: String::new(),
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
        assert!(matches!(missing, Err(PackError::NotFound(_))));

        let by_query = provider
            .resolve_version(&pack, &VersionSelector::Query("1.0".to_string()))
            .await
            .unwrap();
        assert_eq!(by_query.version.id, "pack-1.0.zip");
    }

    #[tokio::test]
    async fn resolve_errors_name_the_local_archive() {
        let dir = tempfile::tempdir().unwrap();
        write_zip(&dir.path().join("pack-1.0.zip"), &[("mods/a.jar", b"a")]);

        let provider = LocalArchiveProvider::new(dir.path().to_path_buf());
        let err = provider
            .resolve_version(&pack_ref(), &VersionSelector::Query("2.0".to_string()))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with(&format!(
                "no version matching '2.0' for local archive '{}'",
                dir.path().display()
            )),
            "{message}"
        );
        assert!(message.ends_with("did you mean: pack-1.0?"), "{message}");
    }

    #[tokio::test]
    async fn prepare_strips_a_wrapper_directory_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("pack-1.0.zip");
        write_zip(
            &zip,
            &[
                ("Server-1.0/mods/a.jar", b"a"),
                ("Server-1.0/eula.txt", b"e"),
            ],
        );

        let provider = LocalArchiveProvider::new(zip);
        let resolved = provider
            .resolve_version(&pack_ref(), &VersionSelector::Latest)
            .await
            .unwrap();
        let staging = tempfile::tempdir().unwrap();
        let prepared = provider.prepare(&resolved, staging.path()).await.unwrap();

        assert!(prepared.root.join("mods/a.jar").is_file());
        assert_eq!(prepared.notices.len(), 1, "{:?}", prepared.notices);
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

    #[tokio::test]
    async fn directory_ignores_everything_that_is_not_a_zip_file() {
        let dir = tempfile::tempdir().unwrap();
        write_zip(&dir.path().join("a.zip"), &[("mods/a.jar", b"a")]);
        std::fs::write(dir.path().join("notes.txt"), b"not a pack").unwrap();
        // Looks like an archive by name only: no .zip extension.
        std::fs::write(dir.path().join("zip"), b"not a pack").unwrap();
        std::fs::write(dir.path().join("b.zip.part"), b"partial download").unwrap();

        let provider = LocalArchiveProvider::new(dir.path().to_path_buf());
        let versions = provider.list_versions(&pack_ref()).await.unwrap();

        assert_eq!(versions.len(), 1, "versions: {versions:?}");
        assert_eq!(versions[0].id, "a.zip");
        assert_eq!(versions[0].name, "a");
    }

    #[tokio::test]
    async fn directory_without_archives_lists_nothing_and_cannot_resolve() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"not a pack").unwrap();

        let provider = LocalArchiveProvider::new(dir.path().to_path_buf());
        let pack = pack_ref();

        // An empty drop folder is a valid state, not a broken profile.
        assert!(provider.list_versions(&pack).await.unwrap().is_empty());

        // But there is nothing to update to, so resolving must fail rather
        // than pick the non-archive file.
        let latest = provider
            .resolve_version(&pack, &VersionSelector::Latest)
            .await;
        assert!(matches!(latest, Err(PackError::NotFound(_))));
    }
    #[tokio::test]
    async fn lists_mrpack_and_uppercase_zip() {
        let dir = tempfile::tempdir().unwrap();
        write_zip(&dir.path().join("a.ZIP"), &[("mods/a.jar", b"a")]);
        std::thread::sleep(Duration::from_millis(50));
        write_mrpack(&dir.path().join("b.mrpack"), &[], &[]);
        std::thread::sleep(Duration::from_millis(50));
        write_mrpack(&dir.path().join("c.MrPack"), &[], &[]);
        std::fs::write(dir.path().join("d.mrpack.part"), b"partial").unwrap();

        let provider = LocalArchiveProvider::new(dir.path().to_path_buf());
        let versions = provider.list_versions(&pack_ref()).await.unwrap();
        assert_eq!(
            versions.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            ["c.MrPack", "b.mrpack", "a.ZIP"]
        );
        assert_eq!(versions[0].name, "c");
    }

    #[tokio::test]
    async fn mrpack_dispatch_uses_mrpack_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live");
        std::fs::create_dir_all(live.join("mods")).unwrap();
        std::fs::write(live.join("mods/kept.jar"), b"kept").unwrap();
        let archive = dir.path().join("Pack-1.0.mrpack");
        let responses = write_mrpack(
            &archive,
            &[
                ("mods/kept.jar", b"kept", "required"),
                ("mods/new.jar", b"new", "required"),
            ],
            &[("overrides/config/c.toml", b"c")],
        );
        let fetcher = Arc::new(FakeFetcher::with(responses));
        let provider = LocalArchiveProvider::new(archive)
            .with_reuse_root(live.clone())
            .with_fetcher(fetcher.clone());

        let resolved = provider
            .resolve_version(&pack_ref(), &VersionSelector::Latest)
            .await
            .unwrap();
        assert_eq!(resolved.version.name, "Pack-1.0");
        let staging = tempfile::tempdir().unwrap();
        let prepared = provider.prepare(&resolved, staging.path()).await.unwrap();

        let mut rels: Vec<String> = prepared
            .files
            .iter()
            .map(|f| f.rel_path.to_string_lossy().replace('\\', "/"))
            .collect();
        rels.sort();
        assert_eq!(rels, ["config/c.toml", "mods/kept.jar", "mods/new.jar"]);
        // Only the file missing from the server was downloaded.
        assert_eq!(fetcher.fetched_names(), ["new.jar"]);
        assert_eq!(
            prepared.requirements.unwrap().describe(),
            "Minecraft 1.21.1 · NeoForge 21.1.77"
        );
        assert_eq!(
            std::fs::read(prepared.root.join("mods/new.jar")).unwrap(),
            b"new"
        );
    }

    #[tokio::test]
    async fn missing_drop_folder_hint_mentions_mrpack() {
        let dir = tempfile::tempdir().unwrap();
        let provider = LocalArchiveProvider::new(dir.path().join("packs"));
        let err = provider.list_versions(&pack_ref()).await.unwrap_err();
        assert!(
            err.to_string()
                .ends_with("create the folder and drop a server-pack .zip or .mrpack into it"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn local_zip_central_directory_fingerprints_match_extracted_files() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("pack-1.0.zip");
        // A wrapper directory, as many server-pack zips have.
        write_zip(
            &zip,
            &[
                ("Server-1.0/mods/a.jar", b"jar a contents"),
                ("Server-1.0/mods/b.jar", b"jar b"),
                ("Server-1.0/mods/nested/c.jar", b"not a mod folder jar"),
                ("Server-1.0/config/a.toml", b"a"),
            ],
        );
        let provider = LocalArchiveProvider::new(zip.clone());
        let versions = provider.list_versions(&pack_ref()).await.unwrap();
        let prints = provider
            .version_fingerprints(&pack_ref(), &versions)
            .await
            .unwrap();
        assert_eq!(prints.len(), 1);
        assert_eq!(prints[0].version, versions[0]);
        let names: Vec<&str> = prints[0]
            .jars
            .iter()
            .map(|jar| jar.file_name.as_str())
            .collect();
        assert_eq!(names, ["a.jar", "b.jar"]);

        // The live server is the extracted pack: every jar matches.
        let staging = tempfile::tempdir().unwrap();
        let resolved = provider
            .resolve_version(&pack_ref(), &VersionSelector::Latest)
            .await
            .unwrap();
        let prepared = provider.prepare(&resolved, staging.path()).await.unwrap();
        let live = crate::core::detect::live_jar_prints(&prepared.root, &prints).unwrap();
        assert_eq!(crate::core::detect::score(&live, &prints[0]), 1.0);
        let a = live.iter().find(|jar| jar.file_name == "a.jar").unwrap();
        assert_eq!(a.crc32, Some(crc32fast::hash(b"jar a contents")));
    }

    #[tokio::test]
    async fn mrpack_fingerprints_use_the_index_and_override_jars() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack-2.0.mrpack");
        write_mrpack(
            &mrpack,
            &[
                ("mods/server.jar", b"server mod", "required"),
                ("mods/client.jar", b"client mod", "unsupported"),
                ("resourcepacks/r.zip", b"pack", "required"),
            ],
            &[("overrides/mods/bundled.jar", b"bundled")],
        );
        let jars = archive_jar_prints(&mrpack).unwrap();
        assert_eq!(jars.len(), 2, "{jars:?}");
        assert_eq!(jars[0].file_name, "bundled.jar");
        assert_eq!(jars[0].crc32, Some(crc32fast::hash(b"bundled")));
        assert_eq!(jars[1].file_name, "server.jar");
        assert_eq!(jars[1].size, 10);
        assert_eq!(
            jars[1].sha1.as_deref(),
            Some(crate::providers::modrinth::provider::fake::sha1_hex(b"server mod").as_str())
        );
    }
}
