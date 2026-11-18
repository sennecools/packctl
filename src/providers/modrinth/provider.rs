//! Modrinth `PackProvider` and the `.mrpack` build pipeline.
//!
//! A version's `.mrpack` is downloaded and verified, then the server tree is
//! built in staging in layers: verified downloads from the index first, then
//! `overrides/`, then `server-overrides/` (a later layer overwrites an
//! earlier one; `client-overrides/` is never used). Files the server does
//! not support are skipped. A file already on the live server with the same
//! size and SHA-512 is copied from there instead of downloaded.
//!
//! packctl does not install mod loaders; the loader and Minecraft versions
//! the pack needs are reported as requirements.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinError;

use crate::core::detect::{JarPrint, VersionFingerprint, jar_hashes};
use crate::core::reserved::{ReservedPaths, display_rel, skipped_notice};
use crate::core::staging::StagingDir;
use crate::error::{PackError, Result};
use crate::fs::extract::{check_prefixed_layers, extract_prefixed_layers, prepared_files_for};
use crate::fs::paths::safe_join;
use crate::providers::http::{
    ExpectedDownload, ExpectedHash, Fetcher, Hashers, check_hashes, create_partial,
};
use crate::providers::{
    PackProvider, PackRef, PackRequirements, PackVersion, PreparedFile, PreparedPack,
    ResolvedPackVersion, VersionSelector, resolve_version_from_list, sanitize,
};

use super::client::MrClient;
use super::models::{MrVersion, is_listed, pack_file, sort_versions_newest_first, version_from_mr};
use super::mrpack::{
    DownloadHosts, OptionalFiles, PlannedDownload, plan_files, read_index, requirements,
    validate_download_url,
};
use super::source::check_server_modpack;
use crate::providers::local::archive_jar_prints;

/// Downloads run at once while building a pack.
pub const DEFAULT_DOWNLOAD_CONCURRENCY: usize = 6;

/// Most optional file names listed in the inclusion notice.
const MAX_OPTIONAL_LISTED: usize = 5;

/// The override layers applied after the downloads, in order.
const OVERRIDE_LAYERS: &[&str] = &["overrides", "server-overrides"];

/// Modrinth-backed pack provider.
pub struct ModrinthProvider {
    pub client: MrClient,
    pub fetcher: Arc<dyn Fetcher>,
    /// The live server root, whose identical files are reused.
    pub reuse_root: Option<PathBuf>,
    pub optional_files: OptionalFiles,
    pub download_concurrency: usize,
    /// Paths the pack may not write; files there are skipped.
    pub reserved: ReservedPaths,
}

/// How [`build_from_mrpack`] gets its files.
pub struct MrpackContext<'a> {
    pub fetcher: &'a dyn Fetcher,
    /// The live server root, whose identical files are reused.
    pub reuse_root: Option<&'a Path>,
    pub optional_files: OptionalFiles,
    pub concurrency: usize,
    /// Paths the pack may not write (the profile's overlay and archive
    /// location besides packctl's control files). Index entries there are
    /// never downloaded and override entries there are never extracted.
    pub reserved: &'a ReservedPaths,
    /// The download hosts index files may use.
    pub download_hosts: DownloadHosts,
}

/// The result of building a server tree from an `.mrpack`.
#[derive(Debug)]
pub struct MrpackOutcome {
    pub files: Vec<PreparedFile>,
    pub requirements: PackRequirements,
    pub notices: Vec<String>,
    /// Files fetched from their download URLs.
    pub downloaded: usize,
    /// Files copied from the live server instead.
    pub reused: usize,
}

/// How one planned file was obtained.
enum Obtained {
    Downloaded,
    Reused,
}

/// Builds the server side of `mrpack` into `server_root` (inside staging).
///
/// The first failing download aborts the whole build; a failed download
/// never leaves a file at its destination.
pub(crate) async fn build_from_mrpack(
    mrpack: &Path,
    server_root: &Path,
    ctx: &MrpackContext<'_>,
) -> Result<MrpackOutcome> {
    let path = mrpack.to_path_buf();
    let optional = ctx.optional_files;
    let hosts = ctx.download_hosts;
    let check_reserved = ctx.reserved.clone();
    let (index, mut plan) = tokio::task::spawn_blocking(move || {
        let index = read_index(&path)?;
        let plan = plan_files(&index, optional, hosts)?;
        // The whole archive and its override layers are checked before
        // anything is downloaded, so a malformed pack fails fast.
        check_prefixed_layers(&path, OVERRIDE_LAYERS, &check_reserved)?;
        Ok::<_, PackError>((index, plan))
    })
    .await
    .map_err(spawn_error)??;
    let (requirements, mut notices) = requirements(&index);

    // Index paths are already free of packctl's control files; the
    // profile's overlay and archive locations are skipped here, before
    // anything is downloaded.
    let mut reserved_skipped = BTreeSet::new();
    plan.downloads.retain(|planned| {
        let reserved = ctx.reserved.is_reserved(&planned.rel_path);
        if reserved {
            reserved_skipped.insert(display_rel(&planned.rel_path));
        }
        !reserved
    });

    tokio::fs::create_dir_all(server_root)
        .await
        .map_err(|e| PackError::io(format!("create '{}'", server_root.display()), e))?;

    let concurrency = ctx.concurrency.max(1);
    // Owned items keep the stream's futures free of higher-ranked lifetimes,
    // which `Send` checking through `async_trait` cannot prove otherwise.
    let obtained: Vec<Obtained> = futures::stream::iter(plan.downloads.clone())
        .map(|planned| async move { obtain(&planned, server_root, ctx).await })
        .buffer_unordered(concurrency)
        .try_collect()
        .await?;
    let reused = obtained
        .iter()
        .filter(|o| matches!(o, Obtained::Reused))
        .count();
    let downloaded = obtained.len() - reused;

    let layers_from = mrpack.to_path_buf();
    let layers_root = server_root.to_path_buf();
    let layers_reserved = ctx.reserved.clone();
    let layers = tokio::task::spawn_blocking(move || {
        extract_prefixed_layers(
            &layers_from,
            &layers_root,
            OVERRIDE_LAYERS,
            &layers_reserved,
        )
    })
    .await
    .map_err(spawn_error)??;
    reserved_skipped.extend(layers.reserved_skipped);

    let mut all: BTreeSet<PathBuf> = plan
        .downloads
        .iter()
        .map(|planned| planned.rel_path.clone())
        .collect();
    all.extend(layers.written);
    let files_root = server_root.to_path_buf();
    let files = tokio::task::spawn_blocking(move || prepared_files_for(&files_root, all))
        .await
        .map_err(spawn_error)??;

    if !plan.client_only.is_empty() {
        notices.push(format!(
            "Skipped {} client-only files",
            plan.client_only.len()
        ));
    }
    if optional == OptionalFiles::Include && !plan.optional_included.is_empty() {
        let mut listed = plan
            .optional_included
            .iter()
            .take(MAX_OPTIONAL_LISTED)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if plan.optional_included.len() > MAX_OPTIONAL_LISTED {
            listed.push_str(", …");
        }
        notices.push(format!(
            "Included {} optional server files: {listed}",
            plan.optional_included.len()
        ));
    }
    if optional == OptionalFiles::Exclude && !plan.optional_excluded.is_empty() {
        notices.push(format!(
            "Excluded {} optional server files",
            plan.optional_excluded.len()
        ));
    }
    if !plan.downloads.is_empty() {
        notices.push(format!(
            "Reused {reused} unchanged files from the server, downloaded {downloaded}"
        ));
    }
    if !reserved_skipped.is_empty() {
        let skipped: Vec<String> = reserved_skipped.into_iter().collect();
        notices.push(skipped_notice(&skipped));
    }

    Ok(MrpackOutcome {
        files,
        requirements,
        notices,
        downloaded,
        reused,
    })
}

/// Puts one planned file at its staging destination: a verified copy of the
/// identical live file when there is one, else a verified download.
async fn obtain(
    planned: &PlannedDownload,
    server_root: &Path,
    ctx: &MrpackContext<'_>,
) -> Result<Obtained> {
    let dest = safe_join(server_root, &planned.rel_path)?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| PackError::io(format!("create '{}'", parent.display()), e))?;
    }
    if let Some(reuse_root) = ctx.reuse_root {
        let live = safe_join(reuse_root, &planned.rel_path)?;
        if reuse_live_file(&live, &dest, planned).await? {
            return Ok(Obtained::Reused);
        }
    }
    let expected = ExpectedDownload {
        size: Some(planned.size),
        hashes: vec![
            ExpectedHash::Sha1(planned.sha1.clone()),
            ExpectedHash::Sha512(planned.sha512.clone()),
        ],
    };
    ctx.fetcher.fetch(&planned.urls, &dest, &expected).await?;
    Ok(Obtained::Downloaded)
}

/// Copies the live file into a partial file next to `dest`, hashing the
/// copy as it is written, and renames it into place only when the copy has
/// the planned size and SHA-512. The copy is what gets checked, never the
/// live file beforehand, so a file that changes while the server runs can
/// not slip through. Returns false (leaving nothing behind) when the live
/// file is missing, not a regular file, or different.
async fn reuse_live_file(live: &Path, dest: &Path, planned: &PlannedDownload) -> Result<bool> {
    let Ok(metadata) = tokio::fs::symlink_metadata(live).await else {
        return Ok(false);
    };
    if !metadata.file_type().is_file() || metadata.len() != planned.size {
        return Ok(false);
    }
    let Ok(mut source) = tokio::fs::File::open(live).await else {
        return Ok(false);
    };
    let expected = ExpectedDownload {
        size: Some(planned.size),
        hashes: vec![ExpectedHash::Sha512(planned.sha512.clone())],
    };
    let (guard, mut output) = create_partial(dest)?;
    let partial = guard.path().to_path_buf();
    let copied = async {
        let mut hashers = Hashers::for_expected(&expected);
        let mut size = 0u64;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = match source.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => read,
                // An unreadable live file is simply not reused.
                Err(_) => return Ok(false),
            };
            size += read as u64;
            if size > planned.size {
                return Ok(false);
            }
            hashers.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .await
                .map_err(|e| PackError::io(format!("write '{}'", partial.display()), e))?;
        }
        output
            .flush()
            .await
            .map_err(|e| PackError::io(format!("flush '{}'", partial.display()), e))?;
        let verified = check_hashes(
            &live.display().to_string(),
            &expected,
            size,
            &hashers.finish(),
        )
        .is_ok();
        Ok::<bool, PackError>(verified)
    }
    .await;
    drop(output);
    match copied {
        Ok(true) => {
            guard.persist(dest).await.map_err(|e| {
                PackError::io(format!("move reused file to '{}'", dest.display()), e)
            })?;
            Ok(true)
        }
        Ok(false) => {
            guard.remove().await?;
            Ok(false)
        }
        // The guard removes the partial file as it drops.
        Err(error) => Err(error),
    }
}

fn spawn_error(error: JoinError) -> PackError {
    PackError::Other(format!("preparation task failed: {error}"))
}

impl ModrinthProvider {
    /// The id or slug to look the project up by.
    fn project_key(pack: &PackRef) -> &str {
        if pack.project_id.trim().is_empty() {
            pack.slug.trim()
        } else {
            pack.project_id.trim()
        }
    }

    fn label(pack: &PackRef) -> String {
        format!("Modrinth project '{}'", Self::project_key(pack))
    }
}

#[async_trait::async_trait]
impl PackProvider for ModrinthProvider {
    async fn list_versions(&self, pack: &PackRef) -> Result<Vec<PackVersion>> {
        let project = self.client.get_project(Self::project_key(pack)).await?;
        check_server_modpack(&project)?;
        let mut versions = self.client.list_versions(&project.id).await?;
        versions.retain(|v| is_listed(v) && pack_file(v).is_some());
        sort_versions_newest_first(&mut versions);
        Ok(versions.iter().map(version_from_mr).collect())
    }

    async fn resolve_version(
        &self,
        pack: &PackRef,
        selector: &VersionSelector,
    ) -> Result<ResolvedPackVersion> {
        let versions = self.list_versions(pack).await?;
        let version = resolve_version_from_list(&versions, selector, &Self::label(pack))?;
        Ok(ResolvedPackVersion {
            pack: pack.clone(),
            version: version.clone(),
        })
    }

    async fn prepare(&self, version: &ResolvedPackVersion, staging: &Path) -> Result<PreparedPack> {
        let mr_version = self.client.get_version(&version.version.id).await?;
        let file = pack_file(&mr_version).ok_or_else(|| {
            PackError::Provider(format!(
                "Modrinth version '{}' has no .mrpack file",
                sanitize(&mr_version.name)
            ))
        })?;
        validate_download_url(&file.url, self.client.download_hosts())?;

        tokio::fs::create_dir_all(staging).await.map_err(|e| {
            PackError::io(
                format!("create staging directory '{}'", staging.display()),
                e,
            )
        })?;
        let mrpack = staging.join("pack.mrpack");
        let expected = ExpectedDownload {
            size: Some(file.size),
            hashes: vec![
                ExpectedHash::Sha1(file.hashes.sha1.trim().to_string()),
                ExpectedHash::Sha512(file.hashes.sha512.trim().to_string()),
            ],
        };
        self.fetcher
            .fetch(std::slice::from_ref(&file.url), &mrpack, &expected)
            .await?;

        let server_root = staging.join("server");
        let ctx = MrpackContext {
            fetcher: self.fetcher.as_ref(),
            reuse_root: self.reuse_root.as_deref(),
            optional_files: self.optional_files,
            concurrency: self.download_concurrency,
            reserved: &self.reserved,
            download_hosts: self.client.download_hosts(),
        };
        let outcome = build_from_mrpack(&mrpack, &server_root, &ctx).await?;
        let name = if version.pack.slug.trim().is_empty() {
            Self::project_key(&version.pack).to_string()
        } else {
            version.pack.slug.clone()
        };
        Ok(PreparedPack {
            name,
            version: version.version.clone(),
            root: server_root,
            files: outcome.files,
            requirements: Some(outcome.requirements),
            notices: outcome.notices,
        })
    }

    /// Fingerprints versions without downloading their packs where possible.
    ///
    /// A modpack version lists the files it bundles as embedded
    /// dependencies. The live server's jars are hashed (SHA-1) and looked up
    /// with one `POST /version_files`; each candidate then counts the
    /// embedded versions found on the server. Only a version without
    /// embedded dependencies is downloaded (when its `.mrpack` is at most
    /// 32 MiB) to read its index.
    ///
    /// Embedded dependencies also cover client-only mods, shaders and
    /// resource packs a server never has. Only dependencies whose project
    /// has a jar on the server (at any version) are scored, so a project
    /// missing from the server does not count against a candidate, while a
    /// server mod at another version does. A dependency without a project
    /// id is always scored.
    async fn version_fingerprints(
        &self,
        pack: &PackRef,
        versions: &[PackVersion],
    ) -> Result<Vec<VersionFingerprint>> {
        let project = self.client.get_project(Self::project_key(pack)).await?;
        let listed = self.client.list_versions(&project.id).await?;
        let by_id: HashMap<&str, &MrVersion> = listed
            .iter()
            .map(|version| (version.id.as_str(), version))
            .collect();

        let live = match &self.reuse_root {
            Some(root) => {
                let root = root.clone();
                tokio::task::spawn_blocking(move || live_jars_by_sha1(&root))
                    .await
                    .map_err(spawn_error)??
            }
            None => HashMap::new(),
        };
        let needs_lookup = versions.iter().any(|version| {
            by_id
                .get(version.id.as_str())
                .is_some_and(|mr| !mr.embedded_versions().is_empty())
        });
        let recognized = if needs_lookup && !live.is_empty() {
            let mut hashes: Vec<String> = live.keys().cloned().collect();
            hashes.sort();
            self.client.versions_by_sha1(&hashes).await?
        } else {
            HashMap::new()
        };
        // Projects with a jar on the server, at whatever version.
        let live_projects: HashSet<&str> = recognized
            .values()
            .map(|version| version.project_id.as_str())
            .collect();
        // Embedded version id -> the SHA-1 of the live jar that is it.
        let found: HashMap<&str, String> = recognized
            .iter()
            .map(|(hash, version)| (version.id.as_str(), hash.to_ascii_lowercase()))
            .collect();

        let mut fingerprints = Vec::with_capacity(versions.len());
        for version in versions {
            let jars = match by_id.get(version.id.as_str()) {
                Some(mr) if !mr.embedded_versions().is_empty() => mr
                    .embedded_versions()
                    .into_iter()
                    .filter(|(_, project)| {
                        project.is_none_or(|project| live_projects.contains(project))
                    })
                    .map(
                        |(id, _)| match found.get(id).and_then(|hash| live.get(hash)) {
                            Some(print) => print.clone(),
                            None => JarPrint {
                                file_name: format!("<{}>", sanitize(id)),
                                size: 0,
                                crc32: None,
                                sha1: None,
                            },
                        },
                    )
                    .collect(),
                Some(mr) => self.downloaded_fingerprint(mr).await?,
                None => Vec::new(),
            };
            fingerprints.push(VersionFingerprint {
                version: version.clone(),
                jars,
            });
        }
        Ok(fingerprints)
    }
}

/// Largest `.mrpack` downloaded only to fingerprint a version.
const MAX_FINGERPRINT_DOWNLOAD: u64 = 32 * 1024 * 1024;

impl ModrinthProvider {
    /// Downloads a small `.mrpack` into a temporary staging directory and
    /// reads the jars it puts into `mods/`. A version without a pack file,
    /// or with one over 32 MiB, yields no jars.
    async fn downloaded_fingerprint(&self, version: &MrVersion) -> Result<Vec<JarPrint>> {
        let Some(file) = pack_file(version) else {
            return Ok(Vec::new());
        };
        if file.size > MAX_FINGERPRINT_DOWNLOAD {
            return Ok(Vec::new());
        }
        validate_download_url(&file.url, self.client.download_hosts())?;
        let staging = StagingDir::create_default()?;
        let mrpack = staging.root.join("pack.mrpack");
        let expected = ExpectedDownload {
            size: Some(file.size),
            hashes: vec![
                ExpectedHash::Sha1(file.hashes.sha1.trim().to_string()),
                ExpectedHash::Sha512(file.hashes.sha512.trim().to_string()),
            ],
        };
        self.fetcher
            .fetch(std::slice::from_ref(&file.url), &mrpack, &expected)
            .await?;
        let jars = tokio::task::spawn_blocking(move || archive_jar_prints(&mrpack))
            .await
            .map_err(spawn_error)??;
        drop(staging);
        Ok(jars)
    }
}

/// The jars directly inside `<root>/mods`, keyed by lowercase SHA-1.
fn live_jars_by_sha1(root: &Path) -> Result<HashMap<String, JarPrint>> {
    let mods = root.join("mods");
    let entries = match std::fs::read_dir(&mods) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(PackError::io(format!("scan '{}'", mods.display()), error)),
    };
    let mut jars = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || !crate::fs::paths::is_jar_path(&path) {
            continue;
        }
        let (_, sha1) = jar_hashes(&path, false, true)?;
        let Some(sha1) = sha1 else {
            continue;
        };
        jars.insert(
            sha1.clone(),
            JarPrint {
                file_name: entry.file_name().to_string_lossy().into_owned(),
                size: metadata.len(),
                crc32: None,
                sha1: Some(sha1),
            },
        );
    }
    Ok(jars)
}

/// A map-backed [`Fetcher`] for tests: serves bytes by URL, verifies them
/// like the real downloader, and records which destinations were fetched.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use std::io::Write;

    use serde_json::json;
    use sha1::Sha1;
    use sha2::{Digest, Sha512};
    use tokio::io::AsyncWriteExt;
    use zip::write::SimpleFileOptions;

    use crate::error::{PackError, Result};
    use crate::providers::http::{
        ExpectedDownload, Fetcher, Hashers, check_hashes, create_partial,
    };

    pub fn sha1_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }

    pub fn sha512_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha512::digest(bytes))
    }

    /// The fake download URL of an index path.
    pub fn url_for(path: &str) -> String {
        format!("https://cdn.modrinth.com/data/test/{path}")
    }

    /// Writes an .mrpack whose index lists `downloads` (path, bytes, server
    /// env) and which carries the given extra zip entries. Returns the
    /// (URL, bytes) responses a [`FakeFetcher`] needs to serve the files.
    pub fn write_mrpack(
        path: &Path,
        downloads: &[(&str, &[u8], &str)],
        entries: &[(&str, &[u8])],
    ) -> Vec<(String, Vec<u8>)> {
        let files: Vec<serde_json::Value> = downloads
            .iter()
            .map(|(rel, bytes, server)| {
                json!({
                    "path": rel,
                    "hashes": { "sha1": sha1_hex(bytes), "sha512": sha512_hex(bytes) },
                    "env": { "client": "required", "server": server },
                    "downloads": [url_for(rel)],
                    "fileSize": bytes.len(),
                })
            })
            .collect();
        let index = json!({
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "1.0.0",
            "name": "Test Pack",
            "files": files,
            "dependencies": { "minecraft": "1.21.1", "neoforge": "21.1.77" },
        });
        let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        writer
            .start_file("modrinth.index.json", SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(serde_json::to_string(&index).unwrap().as_bytes())
            .unwrap();
        for (name, data) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
        downloads
            .iter()
            .map(|(rel, bytes, _)| (url_for(rel), bytes.to_vec()))
            .collect()
    }

    #[derive(Default)]
    pub struct FakeFetcher {
        pub responses: HashMap<String, Vec<u8>>,
        pub fetched: Mutex<Vec<PathBuf>>,
    }

    impl FakeFetcher {
        pub fn with(responses: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
            FakeFetcher {
                responses: responses.into_iter().collect(),
                fetched: Mutex::new(Vec::new()),
            }
        }

        /// Destination file names fetched so far, sorted.
        pub fn fetched_names(&self) -> Vec<String> {
            let mut names: Vec<String> = self
                .fetched
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    #[async_trait::async_trait]
    impl Fetcher for FakeFetcher {
        async fn fetch(
            &self,
            urls: &[String],
            dest: &Path,
            expected: &ExpectedDownload,
        ) -> Result<()> {
            self.fetched.lock().unwrap().push(dest.to_path_buf());
            let url = urls.first().cloned().unwrap_or_default();
            let bytes = urls
                .iter()
                .find_map(|url| self.responses.get(url))
                .ok_or_else(|| PackError::Network(format!("no fake response for {url}")))?;
            let (partial, mut file) = create_partial(dest)?;
            // Yield between writes so concurrent fetches interleave.
            for chunk in bytes.chunks(7) {
                file.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
            file.flush().await.unwrap();
            drop(file);
            let mut hashers = Hashers::for_expected(expected);
            hashers.update(bytes);
            if let Err(error) = check_hashes(&url, expected, bytes.len() as u64, &hashers.finish())
            {
                partial.remove().await?;
                return Err(error);
            }
            partial
                .persist(dest)
                .await
                .map_err(|e| PackError::io("rename fake download", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::fake::{FakeFetcher, sha1_hex, sha512_hex, write_mrpack};
    use super::*;
    use crate::fs::hashing::sha256_bytes;
    use crate::providers::modrinth::models::fixtures;
    use crate::testutil::http_server::{Route, serve};

    static DEFAULT_RESERVED: std::sync::LazyLock<ReservedPaths> =
        std::sync::LazyLock::new(ReservedPaths::default_layout);

    fn ctx<'a>(fetcher: &'a FakeFetcher, reuse_root: Option<&'a Path>) -> MrpackContext<'a> {
        MrpackContext {
            fetcher,
            reuse_root,
            optional_files: OptionalFiles::Include,
            concurrency: DEFAULT_DOWNLOAD_CONCURRENCY,
            reserved: &DEFAULT_RESERVED,
            download_hosts: DownloadHosts::Public,
        }
    }

    fn files_by_path(files: &[PreparedFile]) -> BTreeMap<String, &PreparedFile> {
        files
            .iter()
            .map(|f| (f.rel_path.to_string_lossy().replace('\\', "/"), f))
            .collect()
    }

    /// Every file name under `dir`, recursively.
    fn all_names(dir: &Path) -> Vec<String> {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn build_from_mrpack_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack.mrpack");
        let responses = write_mrpack(
            &mrpack,
            &[
                ("mods/a.jar", b"jar a", "required"),
                ("mods/opt.jar", b"optional", "optional"),
                ("mods/client.jar", b"client only", "unsupported"),
                ("config/x.json", b"{\"from\":\"download\"}", "required"),
            ],
            &[
                ("overrides/config/x.json", b"{\"from\":\"overrides\"}"),
                ("overrides/config/y.toml", b"y = 1"),
                ("server-overrides/config/y.toml", b"y = 2"),
                ("client-overrides/options.txt", b"client"),
            ],
        );
        let fetcher = FakeFetcher::with(responses);
        let server = dir.path().join("staging").join("server");

        let outcome = build_from_mrpack(&mrpack, &server, &ctx(&fetcher, None))
            .await
            .unwrap();

        let files = files_by_path(&outcome.files);
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec![
                "config/x.json",
                "config/y.toml",
                "mods/a.jar",
                "mods/opt.jar"
            ]
        );
        assert_eq!(files["mods/a.jar"].sha256, sha256_bytes(b"jar a"));
        assert_eq!(files["mods/a.jar"].size, 5);
        // overrides beat downloads; server-overrides beat overrides.
        assert_eq!(
            files["config/x.json"].sha256,
            sha256_bytes(b"{\"from\":\"overrides\"}")
        );
        assert_eq!(
            std::fs::read(server.join("config/y.toml")).unwrap(),
            b"y = 2"
        );
        assert!(!server.join("mods/client.jar").exists());
        assert!(!server.join("options.txt").exists());
        assert_eq!(outcome.downloaded, 3);
        assert_eq!(outcome.reused, 0);
        assert_eq!(
            outcome.requirements.describe(),
            "Minecraft 1.21.1 · NeoForge 21.1.77"
        );
        assert_eq!(
            outcome.notices,
            vec![
                "Skipped 1 client-only files".to_string(),
                "Included 1 optional server files: mods/opt.jar".to_string(),
                "Reused 0 unchanged files from the server, downloaded 3".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn optional_files_can_be_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack.mrpack");
        let responses = write_mrpack(
            &mrpack,
            &[
                ("mods/a.jar", b"a", "required"),
                ("mods/opt.jar", b"o", "optional"),
            ],
            &[],
        );
        let fetcher = FakeFetcher::with(responses);
        let context = MrpackContext {
            optional_files: OptionalFiles::Exclude,
            ..ctx(&fetcher, None)
        };
        let outcome = build_from_mrpack(&mrpack, &dir.path().join("server"), &context)
            .await
            .unwrap();
        assert_eq!(outcome.files.len(), 1);
        assert!(
            outcome
                .notices
                .contains(&"Excluded 1 optional server files".to_string()),
            "{:?}",
            outcome.notices
        );
    }

    #[tokio::test]
    async fn sha512_mismatch_aborts_prepare_and_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack.mrpack");
        let mut responses = write_mrpack(
            &mrpack,
            &[("mods/a.jar", b"good bytes", "required")],
            &[("overrides/config/c.toml", b"c")],
        );
        // Same size, different content: sha1 and sha512 fail.
        responses[0].1 = b"evil bytes".to_vec();
        let fetcher = FakeFetcher::with(responses);
        let server = dir.path().join("server");

        let err = build_from_mrpack(&mrpack, &server, &ctx(&fetcher, None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("verification"), "{err}");
        assert!(!server.join("mods/a.jar").exists());
        assert!(
            !all_names(&server)
                .iter()
                .any(|name| name.contains("partial")),
            "{:?}",
            all_names(&server)
        );
        // The overrides were never applied.
        assert!(!server.join("config/c.toml").exists());
    }

    /// Partial files left anywhere under `root`, waiting briefly for removals
    /// of files another task still holds open (Windows may report those
    /// until the handle closes).
    async fn partials_left(root: &Path) -> Vec<String> {
        let partials = || -> Vec<String> {
            all_names(root)
                .into_iter()
                .filter(|name| name.ends_with(".packctl-partial"))
                .collect()
        };
        for _ in 0..100 {
            if partials().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        partials()
    }

    /// Starts `slow.jar` (a partial file with some bytes, then never
    /// finishes) and fails `broken.jar` only once that download is under way.
    struct StallThenFail {
        started: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl Fetcher for StallThenFail {
        async fn fetch(
            &self,
            urls: &[String],
            dest: &Path,
            _expected: &ExpectedDownload,
        ) -> Result<()> {
            if urls[0].ends_with("slow.jar") {
                let (_partial, mut file) = crate::providers::http::create_partial(dest)?;
                file.write_all(b"some bytes").await.unwrap();
                file.flush().await.unwrap();
                self.started.notify_one();
                std::future::pending::<()>().await;
                unreachable!()
            }
            self.started.notified().await;
            Err(PackError::Network("broken download".to_string()))
        }
    }

    #[tokio::test]
    async fn failed_download_cancels_others_without_leaving_partials() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack.mrpack");
        write_mrpack(
            &mrpack,
            &[
                ("mods/slow.jar", b"slow bytes", "required"),
                ("mods/broken.jar", b"broken bytes", "required"),
            ],
            &[],
        );
        let fetcher = StallThenFail {
            started: tokio::sync::Notify::new(),
        };
        let context = MrpackContext {
            fetcher: &fetcher,
            reuse_root: None,
            optional_files: OptionalFiles::Include,
            concurrency: DEFAULT_DOWNLOAD_CONCURRENCY,
            reserved: &DEFAULT_RESERVED,
            download_hosts: DownloadHosts::Public,
        };
        let server = dir.path().join("server");

        let err = build_from_mrpack(&mrpack, &server, &context)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("broken download"), "{err}");
        assert!(!server.join("mods/slow.jar").exists());
        assert_eq!(partials_left(&server).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn reuses_identical_live_file_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live");
        std::fs::create_dir_all(live.join("mods")).unwrap();
        std::fs::write(live.join("mods/same.jar"), b"same bytes").unwrap();
        // Same size, different bytes: must be downloaded.
        std::fs::write(live.join("mods/changed.jar"), b"old!").unwrap();

        let mrpack = dir.path().join("pack.mrpack");
        let responses = write_mrpack(
            &mrpack,
            &[
                ("mods/same.jar", b"same bytes", "required"),
                ("mods/changed.jar", b"new!", "required"),
                ("mods/new.jar", b"brand new", "required"),
            ],
            &[],
        );
        let fetcher = FakeFetcher::with(responses);
        let server = dir.path().join("server");

        let outcome = build_from_mrpack(&mrpack, &server, &ctx(&fetcher, Some(&live)))
            .await
            .unwrap();
        assert_eq!(fetcher.fetched_names(), vec!["changed.jar", "new.jar"]);
        assert_eq!(outcome.reused, 1);
        assert_eq!(outcome.downloaded, 2);
        assert_eq!(
            std::fs::read(server.join("mods/same.jar")).unwrap(),
            b"same bytes"
        );
        assert_eq!(
            std::fs::read(server.join("mods/changed.jar")).unwrap(),
            b"new!"
        );
        // The live tree is untouched.
        assert_eq!(
            std::fs::read(live.join("mods/changed.jar")).unwrap(),
            b"old!"
        );
        assert!(
            outcome
                .notices
                .contains(&"Reused 1 unchanged files from the server, downloaded 2".to_string())
        );
        assert!(!all_names(&server).iter().any(|n| n.contains("partial")));
    }

    #[tokio::test]
    async fn same_stem_files_download_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack.mrpack");
        let toml = vec![b't'; 4096];
        let json = vec![b'j'; 4096];
        let responses = write_mrpack(
            &mrpack,
            &[
                ("config/a.toml", &toml, "required"),
                ("config/a.json", &json, "required"),
                ("config/a.partial", b"literal partial", "required"),
            ],
            &[],
        );
        let fetcher = FakeFetcher::with(responses);
        let server = dir.path().join("server");
        build_from_mrpack(&mrpack, &server, &ctx(&fetcher, None))
            .await
            .unwrap();
        assert_eq!(std::fs::read(server.join("config/a.toml")).unwrap(), toml);
        assert_eq!(std::fs::read(server.join("config/a.json")).unwrap(), json);
        assert_eq!(
            std::fs::read(server.join("config/a.partial")).unwrap(),
            b"literal partial"
        );
        let mut names = all_names(&server);
        names.sort();
        assert_eq!(names, vec!["a.json", "a.partial", "a.toml"]);
    }

    #[tokio::test]
    async fn control_files_in_overrides_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack.mrpack");
        let responses = write_mrpack(
            &mrpack,
            &[("mods/a.jar", b"a", "required")],
            &[("overrides/.packctl.toml", b"evil")],
        );
        let fetcher = FakeFetcher::with(responses);
        let err = build_from_mrpack(&mrpack, &dir.path().join("server"), &ctx(&fetcher, None))
            .await
            .unwrap_err();
        assert!(matches!(err, PackError::UnsafePath(_)), "{err:?}");
        // Refused before anything was downloaded.
        assert!(fetcher.fetched_names().is_empty());
    }

    #[tokio::test]
    async fn unsafe_archive_entries_are_refused_before_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack.mrpack");
        let responses = write_mrpack(
            &mrpack,
            &[("mods/a.jar", b"a", "required")],
            &[
                ("overrides/config/a.toml", b"a"),
                ("server-overrides/../escape.toml", b"evil"),
            ],
        );
        let fetcher = FakeFetcher::with(responses);
        let server = dir.path().join("server");
        let err = build_from_mrpack(&mrpack, &server, &ctx(&fetcher, None))
            .await
            .unwrap_err();
        assert!(
            matches!(err, PackError::UnsafePathComponent { .. }),
            "{err:?}"
        );
        assert!(fetcher.fetched_names().is_empty());
        assert!(all_names(&server).is_empty());
    }

    #[tokio::test]
    async fn profile_reserved_paths_are_skipped_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("pack.mrpack");
        let responses = write_mrpack(
            &mrpack,
            &[
                ("mods/a.jar", b"a", "required"),
                ("config/my-overlay/mods/x.jar", b"evil", "required"),
            ],
            &[
                ("overrides/config/c.toml", b"c"),
                ("server-overrides/drops/pack.zip", b"evil"),
            ],
        );
        let fetcher = FakeFetcher::with(responses);
        let server = dir.path().join("server");
        let reserved = ReservedPaths::control_files_and(&["config/my-overlay", "drops/pack.zip"]);
        let context = MrpackContext {
            reserved: &reserved,
            ..ctx(&fetcher, None)
        };

        let outcome = build_from_mrpack(&mrpack, &server, &context).await.unwrap();
        assert_eq!(fetcher.fetched_names(), vec!["a.jar"]);
        assert_eq!(
            files_by_path(&outcome.files)
                .into_keys()
                .collect::<Vec<_>>(),
            vec!["config/c.toml", "mods/a.jar"]
        );
        assert!(!server.join("config/my-overlay").exists());
        assert!(!server.join("drops").exists());
        assert!(
            outcome.notices.contains(
                &"Skipped 2 pack file(s) in locations packctl reserves for itself\n  \
                  config/my-overlay/mods/x.jar\n  drops/pack.zip"
                    .to_string()
            ),
            "{:?}",
            outcome.notices
        );
    }

    #[tokio::test]
    async fn provider_lists_resolves_and_prepares() {
        let dir = tempfile::tempdir().unwrap();
        let mrpack = dir.path().join("source.mrpack");
        let mut responses = write_mrpack(&mrpack, &[("mods/a.jar", b"a", "required")], &[]);
        let pack_bytes = std::fs::read(&mrpack).unwrap();
        let pack_url = "https://cdn.modrinth.com/data/5FFgwNNP/versions/YQpPe5Iu/pack.mrpack";
        responses.push((pack_url.to_string(), pack_bytes.clone()));

        // The version as the API reports it, pointing at the built pack.
        let mut versions: serde_json::Value = serde_json::from_str(fixtures::VERSIONS).unwrap();
        let file = &mut versions[0]["files"][0];
        file["url"] = json!(pack_url);
        file["size"] = json!(pack_bytes.len());
        file["hashes"] =
            json!({ "sha1": sha1_hex(&pack_bytes), "sha512": sha512_hex(&pack_bytes) });
        let version = versions[0].to_string();

        let base = serve(vec![
            Route::json("/v2/project/5FFgwNNP", fixtures::PROJECT),
            Route::json("/v2/project/5FFgwNNP/version", &versions.to_string()),
            Route::json("/v2/version/YQpPe5Iu", &version),
        ])
        .await;
        let provider = ModrinthProvider {
            client: MrClient::new(&format!("{base}/v2")).unwrap(),
            fetcher: Arc::new(FakeFetcher::with(responses)),
            reuse_root: None,
            optional_files: OptionalFiles::Include,
            download_concurrency: 2,
            reserved: ReservedPaths::default_layout(),
        };
        let pack = PackRef {
            project_id: "5FFgwNNP".to_string(),
            slug: "cobblemon-modpack".to_string(),
        };

        let listed = provider.list_versions(&pack).await.unwrap();
        assert_eq!(
            listed.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            vec!["YQpPe5Iu", "Pz8kd2Lm"]
        );
        let resolved = provider
            .resolve_version(&pack, &VersionSelector::Query("1.6.1".to_string()))
            .await
            .unwrap();
        assert_eq!(resolved.version.id, "YQpPe5Iu");

        let staging = tempfile::tempdir().unwrap();
        let prepared = provider.prepare(&resolved, staging.path()).await.unwrap();
        assert_eq!(prepared.name, "cobblemon-modpack");
        assert_eq!(prepared.files.len(), 1);
        assert_eq!(prepared.root, staging.path().join("server"));
        assert_eq!(
            prepared.requirements.unwrap().minecraft.as_deref(),
            Some("1.21.1")
        );
    }

    #[tokio::test]
    async fn fingerprints_count_embedded_versions_found_on_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server");
        std::fs::create_dir_all(server.join("mods")).unwrap();
        std::fs::write(server.join("mods/found.jar"), b"found jar").unwrap();
        std::fs::write(server.join("mods/admin.jar"), b"admin jar").unwrap();
        let found_sha1 = sha1_hex(b"found jar");
        let admin_sha1 = sha1_hex(b"admin jar");

        // Version C embeds nothing, so its small .mrpack is read instead.
        let mrpack = dir.path().join("c.mrpack");
        let mut responses = write_mrpack(
            &mrpack,
            &[("mods/found.jar", b"found jar", "required")],
            &[],
        );
        let pack_bytes = std::fs::read(&mrpack).unwrap();
        let pack_url = "https://cdn.modrinth.com/data/5FFgwNNP/versions/CCCCCCCC/c.mrpack";
        responses.push((pack_url.to_string(), pack_bytes.clone()));

        let version =
            |id: &str, date: &str, file: serde_json::Value, embedded: &[(&str, Option<&str>)]| {
                json!({
                    "id": id,
                    "project_id": "5FFgwNNP",
                    "name": format!("Pack {id}"),
                    "version_number": id,
                    "date_published": date,
                    "version_type": "release",
                    "status": "listed",
                    "files": [file],
                    "dependencies": embedded.iter().map(|(dep, project)| json!({
                        "version_id": dep,
                        "project_id": project,
                        "file_name": null,
                        "dependency_type": "embedded",
                    })).collect::<Vec<_>>(),
                })
            };
        let big_file = |name: &str| {
            json!({
                "hashes": { "sha1": "0".repeat(40), "sha512": "0".repeat(128) },
                "url": format!("https://cdn.modrinth.com/data/5FFgwNNP/{name}.mrpack"),
                "filename": format!("{name}.mrpack"),
                "primary": true,
                "size": 100_855_720u64,
            })
        };
        let versions = json!([
            version(
                "AAAAAAAA",
                "2025-03-01T00:00:00Z",
                big_file("a"),
                // Found; a server mod at another version; a dependency
                // without a project id; a client-only mod (not scored).
                &[
                    ("FOUND111", Some("modproj1")),
                    ("OLDADMIN", Some("modproj2")),
                    ("GONE2222", None),
                    ("CLIENT33", Some("clientproj")),
                ]
            ),
            version(
                "BBBBBBBB",
                "2025-02-01T00:00:00Z",
                big_file("b"),
                // The client-only mod and shader pack never reach a server.
                &[
                    ("FOUND111", Some("modproj1")),
                    ("ADMIN222", Some("modproj2")),
                    ("CLIENT33", Some("clientproj")),
                    ("SHADER44", Some("shaderproj")),
                ]
            ),
            version(
                "CCCCCCCC",
                "2025-01-01T00:00:00Z",
                json!({
                    "hashes": { "sha1": sha1_hex(&pack_bytes), "sha512": sha512_hex(&pack_bytes) },
                    "url": pack_url,
                    "filename": "c.mrpack",
                    "primary": true,
                    "size": pack_bytes.len(),
                }),
                &[]
            ),
        ]);
        let version_files = json!({
            found_sha1.clone(): {
                "id": "FOUND111",
                "project_id": "modproj1",
                "name": "Found 1.0",
                "version_number": "1.0",
                "date_published": "2024-01-01T00:00:00Z",
                "version_type": "release",
            },
            admin_sha1.clone(): {
                "id": "ADMIN222",
                "project_id": "modproj2",
                "name": "Admin 2.0",
                "version_number": "2.0",
                "date_published": "2024-01-01T00:00:00Z",
                "version_type": "release",
            }
        });
        let base = serve(vec![
            Route::json("/v2/project/5FFgwNNP", fixtures::PROJECT),
            Route::json("/v2/project/5FFgwNNP/version", &versions.to_string()),
            Route::json("/v2/version_files", &version_files.to_string()),
        ])
        .await;
        let fetcher = Arc::new(FakeFetcher::with(responses));
        let provider = ModrinthProvider {
            client: MrClient::new(&format!("{base}/v2")).unwrap(),
            fetcher: fetcher.clone(),
            reuse_root: Some(server.clone()),
            optional_files: OptionalFiles::Include,
            download_concurrency: 2,
            reserved: ReservedPaths::default_layout(),
        };
        let pack = PackRef {
            project_id: "5FFgwNNP".to_string(),
            slug: String::new(),
        };

        let listed = provider.list_versions(&pack).await.unwrap();
        let prints = provider.version_fingerprints(&pack, &listed).await.unwrap();
        let live = crate::core::detect::live_jar_prints(&server, &prints).unwrap();
        let scores: Vec<(String, f64)> = prints
            .iter()
            .map(|print| {
                (
                    print.version.id.clone(),
                    crate::core::detect::score(&live, print),
                )
            })
            .collect();
        assert_eq!(
            scores,
            vec![
                ("AAAAAAAA".to_string(), 1.0 / 3.0),
                ("BBBBBBBB".to_string(), 1.0),
                ("CCCCCCCC".to_string(), 1.0),
            ]
        );
        // Only the small pack without embedded dependencies was downloaded.
        assert_eq!(fetcher.fetched_names(), ["pack.mrpack"]);
    }
}
