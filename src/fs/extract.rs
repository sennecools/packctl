//! Safe zip extraction shared by pack providers.
//!
//! Archive entry paths are treated as untrusted: absolute paths, `..`
//! components, `.`/empty components, NUL bytes, backslash traversal, symlink
//! entries, duplicate normalized paths, and archives that exceed hard size or
//! entry-count limits abort the whole extraction with a [`crate::error::PackError`].

use std::collections::{BTreeSet, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::profile::LOCAL_PROFILE;
use crate::core::reserved::{IGNORE_FILE, ReservedPaths, STATE_DIR, display_rel, skipped_notice};
use crate::error::{PackError, Result};
use crate::fs::hashing::sha256_file;
use crate::fs::paths::{normalize_relative, safe_join};
use crate::providers::PreparedFile;

/// The result of extracting a server pack.
#[derive(Debug)]
pub(crate) struct ExtractedPack {
    /// One entry per extracted file, relative to the extraction root.
    pub files: Vec<PreparedFile>,
    /// The single top-level wrapper directory that was stripped, if any.
    pub wrapper: Option<String>,
    /// Entries skipped because they target a reserved path (forward-slash
    /// relative paths, sorted). They were never written.
    pub reserved_skipped: Vec<String>,
}

impl ExtractedPack {
    /// Plan notices describing how the archive was read.
    pub fn notices(&self) -> Vec<String> {
        let mut notices: Vec<String> = self
            .wrapper
            .iter()
            .map(|wrapper| {
                format!(
                    "Archive wraps the server in a '{wrapper}/' folder; its contents were installed at the server root"
                )
            })
            .collect();
        if !self.reserved_skipped.is_empty() {
            notices.push(skipped_notice(&self.reserved_skipped));
        }
        notices
    }
}

/// The result of [`extract_prefixed_layers`].
#[derive(Debug)]
pub(crate) struct ExtractedLayers {
    /// The relative paths written.
    pub written: BTreeSet<PathBuf>,
    /// Entries skipped because they target a reserved path (forward-slash
    /// relative paths, sorted). They were never written.
    pub reserved_skipped: Vec<String>,
}

/// Top-level directory names that belong to a server pack itself. A folder
/// with one of these names is never treated as a wrapper around the pack.
const PACK_DIRS: &[&str] = &[
    "mods",
    "config",
    "kubejs",
    "defaultconfigs",
    "scripts",
    "resourcepacks",
];

/// Directories whose presence directly under a candidate wrapper shows that
/// the wrapper holds a server pack.
const WRAPPED_PACK_MARKERS: &[&str] = &["mods", "config"];

/// Detects a single wrapper directory around a server pack.
///
/// Many server-pack zips put everything under one folder such as
/// `ServerFiles-1.2/`. `names` are the normalized paths of the archive's
/// files (directory entries excluded). Returns the wrapper name `W` when
/// every file lies under `W/`, `W` is not itself a pack directory (so a
/// config-only zip with `config/kubejs/...` is left alone), and `W` directly
/// contains `mods/` or `config/`.
pub(crate) fn common_wrapper(names: &[PathBuf]) -> Option<String> {
    let mut wrapper: Option<&str> = None;
    let mut has_marker = false;
    for name in names {
        let mut components = name.components();
        let first = components.next()?.as_os_str().to_str()?;
        // A file directly at the top level means there is no wrapper.
        let second = components.next()?.as_os_str().to_str()?;
        match wrapper {
            None => wrapper = Some(first),
            Some(existing) if existing == first => {}
            Some(_) => return None,
        }
        if components.next().is_some() && WRAPPED_PACK_MARKERS.contains(&second) {
            has_marker = true;
        }
    }
    let wrapper = wrapper?;
    if !has_marker || PACK_DIRS.contains(&wrapper) {
        return None;
    }
    Some(wrapper.to_string())
}

/// Removes the wrapper directory from a normalized entry path. Returns None
/// for the wrapper directory entry itself.
fn strip_wrapper(rel: PathBuf, wrapper: Option<&str>) -> Option<PathBuf> {
    let Some(wrapper) = wrapper else {
        return Some(rel);
    };
    match rel.strip_prefix(wrapper) {
        Ok(rest) if rest.as_os_str().is_empty() => None,
        Ok(rest) => Some(rest.to_path_buf()),
        Err(_) => Some(rel),
    }
}

/// Extracts a server pack zip into `dest_root`, enforcing strict path safety.
///
/// Absolute paths, `..` components, `.`/empty components, NUL bytes, backslash
/// traversal, and symlink entries abort the whole extraction with a
/// `PackError`; unsafe entries are never silently skipped. Directory entries
/// are skipped. A single wrapper directory around the pack is stripped (see
/// [`common_wrapper`]). Entries whose path (after stripping the wrapper) is
/// `reserved` are not written and are listed in
/// [`ExtractedPack::reserved_skipped`]; the planner skips such paths too, so
/// this is a second line of defense. Returns a `PreparedFile` for every
/// extracted file.
///
/// This is CPU/IO-heavy synchronous code and should be called inside
/// `spawn_blocking` by async callers.
pub(crate) fn extract_server_pack(
    zip_path: &Path,
    dest_root: &Path,
    reserved: &ReservedPaths,
) -> Result<ExtractedPack> {
    std::fs::create_dir_all(dest_root)
        .map_err(|e| PackError::io(format!("create '{}'", dest_root.display()), e))?;

    let archive_file = std::fs::File::open(zip_path)
        .map_err(|e| PackError::io(format!("open '{}'", zip_path.display()), e))?;
    let mut archive = zip::ZipArchive::new(archive_file).map_err(|e| {
        PackError::Parse(format!("invalid zip archive '{}': {e}", zip_path.display()))
    })?;

    let wrapper = validate_archive(&mut archive, zip_path)?;
    let mut dest_rels = Vec::new();
    let mut reserved_skipped = Vec::new();
    let mut total_uncompressed = 0u64;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| {
            PackError::Parse(format!(
                "read entry {index} from '{}': {e}",
                zip_path.display()
            ))
        })?;

        if entry.is_dir() {
            continue;
        }

        let name = entry_name(&entry, zip_path)?;
        let rel = normalize_relative(Path::new(&name))?;
        let Some(rel) = strip_wrapper(rel, wrapper.as_deref()) else {
            continue;
        };
        if reserved.is_reserved(&rel) {
            reserved_skipped.push(display_rel(&rel));
            continue;
        }
        let dest = safe_join(dest_root, &rel)?;
        write_entry(&mut entry, &name, &dest, &mut total_uncompressed)?;
        dest_rels.push(rel);
    }
    reserved_skipped.sort();

    Ok(ExtractedPack {
        files: prepared_files_for(dest_root, dest_rels)?,
        wrapper,
        reserved_skipped,
    })
}

/// Hashes and sizes the files at `rels` (relative to `dest_root`) into
/// [`PreparedFile`]s, in the order given.
///
/// Every path goes through [`safe_join`], so a caller can never describe a
/// file outside `dest_root`. Callers pass only paths they wrote, which the
/// extraction and `.mrpack` code already keep away from reserved locations;
/// the planner skips reserved paths as well. This is blocking IO.
pub(crate) fn prepared_files_for(
    dest_root: &Path,
    rels: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<PreparedFile>> {
    let mut prepared = Vec::new();
    for rel in rels {
        let rel = normalize_relative(&rel)?;
        let dest = safe_join(dest_root, &rel)?;
        let sha256 = sha256_file(&dest)?;
        let size = std::fs::metadata(&dest)
            .map_err(|e| PackError::io(format!("stat '{}'", dest.display()), e))?
            .len();
        prepared.push(PreparedFile {
            rel_path: rel,
            size,
            sha256,
        });
    }
    Ok(prepared)
}

/// Extracts the entries under each of `prefixes` (top-level folders such as
/// `overrides` and `server-overrides`) into `dest_root`, with the prefix
/// stripped, in the order given. Entries outside every prefix are skipped.
///
/// The whole archive is checked first, exactly like [`extract_server_pack`]
/// (entry count, path safety, symlinks, size limits). A path that appears
/// twice within one prefix is an error; a later prefix overwrites an earlier
/// one. Pack content may never write packctl's own control files (see
/// [`is_control_path`]): such an entry is an error. Other `reserved` paths
/// (the profile's overlay or archive location) are skipped and listed in
/// [`ExtractedLayers::reserved_skipped`].
///
/// Blocking IO; async callers use `spawn_blocking`.
pub(crate) fn extract_prefixed_layers(
    zip_path: &Path,
    dest_root: &Path,
    prefixes: &[&str],
    reserved: &ReservedPaths,
) -> Result<ExtractedLayers> {
    std::fs::create_dir_all(dest_root)
        .map_err(|e| PackError::io(format!("create '{}'", dest_root.display()), e))?;
    let (mut archive, (layers, reserved_skipped)) =
        plan_prefixed_layers(zip_path, prefixes, reserved)?;

    let mut written = BTreeSet::new();
    let mut total_uncompressed = 0u64;
    for layer in layers {
        for (index, rel) in layer {
            let mut entry = archive.by_index(index).map_err(|e| {
                PackError::Parse(format!(
                    "read entry {index} from '{}': {e}",
                    zip_path.display()
                ))
            })?;
            let name = entry_name(&entry, zip_path)?;
            let dest = safe_join(dest_root, &rel)?;
            write_entry(&mut entry, &name, &dest, &mut total_uncompressed)?;
            written.insert(rel);
        }
    }
    Ok(ExtractedLayers {
        written,
        reserved_skipped: reserved_skipped.into_iter().collect(),
    })
}

/// Runs every check [`extract_prefixed_layers`] makes before it writes
/// anything (entry count, path safety, symlinks, size limits, duplicates
/// and control-file paths), without extracting. Callers that do expensive
/// work before extracting (such as downloading an `.mrpack`'s files) call
/// it first, so a malformed archive fails before any of that.
///
/// Blocking IO; async callers use `spawn_blocking`.
pub(crate) fn check_prefixed_layers(
    zip_path: &Path,
    prefixes: &[&str],
    reserved: &ReservedPaths,
) -> Result<()> {
    plan_prefixed_layers(zip_path, prefixes, reserved).map(|_| ())
}

/// The entries of each layer to extract, as `(index, path without the
/// prefix)`, and the reserved paths skipped.
type LayerPlan = (Vec<Vec<(usize, PathBuf)>>, BTreeSet<String>);

/// Opens and checks the whole archive, then plans every layer, so a
/// duplicate or a control-file path aborts with nothing extracted.
fn plan_prefixed_layers(
    zip_path: &Path,
    prefixes: &[&str],
    reserved: &ReservedPaths,
) -> Result<(zip::ZipArchive<std::fs::File>, LayerPlan)> {
    let archive_file = std::fs::File::open(zip_path)
        .map_err(|e| PackError::io(format!("open '{}'", zip_path.display()), e))?;
    let mut archive = zip::ZipArchive::new(archive_file).map_err(|e| {
        PackError::Parse(format!("invalid zip archive '{}': {e}", zip_path.display()))
    })?;
    validate_archive(&mut archive, zip_path)?;
    let plan = plan_layers(&mut archive, zip_path, prefixes, reserved)?;
    Ok((archive, plan))
}

fn plan_layers(
    archive: &mut zip::ZipArchive<std::fs::File>,
    zip_path: &Path,
    prefixes: &[&str],
    reserved: &ReservedPaths,
) -> Result<LayerPlan> {
    let mut layers: Vec<Vec<(usize, PathBuf)>> = Vec::with_capacity(prefixes.len());
    let mut reserved_skipped = BTreeSet::new();
    for prefix in prefixes {
        let mut seen = HashSet::new();
        let mut layer = Vec::new();
        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(|e| {
                PackError::Parse(format!(
                    "read entry {index} from '{}': {e}",
                    zip_path.display()
                ))
            })?;
            if entry.is_dir() {
                continue;
            }
            let name = entry_name(&entry, zip_path)?;
            let rel = normalize_relative(Path::new(&name))?;
            let Ok(rest) = rel.strip_prefix(prefix) else {
                continue;
            };
            if rest.as_os_str().is_empty() {
                continue;
            }
            let rest = rest.to_path_buf();
            if is_control_path(&rest) {
                return Err(PackError::UnsafePath(rel));
            }
            if !seen.insert(rest.clone()) {
                return Err(PackError::Provider(format!(
                    "zip archive '{}' contains a duplicate path '{}'",
                    zip_path.display(),
                    rel.display()
                )));
            }
            if reserved.is_reserved(&rest) {
                reserved_skipped.insert(display_rel(&rest));
                continue;
            }
            layer.push((index, rest));
        }
        layers.push(layer);
    }
    Ok((layers, reserved_skipped))
}

/// True when `rel` (relative to the server root) names one of packctl's own
/// control files: the state directory, the local profile or the ignore file.
/// Compared case-insensitively, since some filesystems are.
pub(crate) fn is_control_path(rel: &Path) -> bool {
    let Some(first) = rel.components().next() else {
        return false;
    };
    let first = first.as_os_str().to_string_lossy().to_ascii_lowercase();
    [STATE_DIR, LOCAL_PROFILE, IGNORE_FILE].contains(&first.as_str())
}

/// Streams one zip entry into `dest`, enforcing the per-entry and running
/// total size limits.
fn write_entry<R: Read + ?Sized>(
    entry: &mut zip::read::ZipFile<'_, R>,
    name: &str,
    dest: &Path,
    total_uncompressed: &mut u64,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| PackError::io(format!("create '{}'", parent.display()), e))?;
    }
    let mut out = std::fs::File::create(dest)
        .map_err(|e| PackError::io(format!("create '{}'", dest.display()), e))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut entry_uncompressed = 0u64;
    loop {
        let read = entry
            .read(&mut buffer)
            .map_err(|e| PackError::io(format!("read zip entry '{name}'"), e))?;
        if read == 0 {
            break;
        }
        entry_uncompressed = entry_uncompressed
            .checked_add(read as u64)
            .ok_or_else(|| PackError::Provider("zip entry size overflow".to_string()))?;
        *total_uncompressed = total_uncompressed
            .checked_add(read as u64)
            .ok_or_else(|| PackError::Provider("zip total size overflow".to_string()))?;
        if entry_uncompressed > MAX_ENTRY_UNCOMPRESSED_BYTES
            || *total_uncompressed > MAX_TOTAL_UNCOMPRESSED_BYTES
        {
            return Err(PackError::Provider(
                "zip archive exceeds extraction size limits".to_string(),
            ));
        }
        out.write_all(&buffer[..read])
            .map_err(|e| PackError::io(format!("write '{}'", dest.display()), e))?;
    }
    Ok(())
}

const MAX_ZIP_ENTRIES: usize = 10_000;
/// Most bytes one pack may unpack to, in total.
pub(crate) const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Most bytes one file of a pack may unpack to.
pub(crate) const MAX_ENTRY_UNCOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;

/// Checks entry count, path safety, symlinks, duplicates and size limits
/// before anything is written, and returns the wrapper directory to strip.
///
/// Duplicate detection runs on the paths as they will be extracted, after
/// the wrapper is stripped.
fn validate_archive(
    archive: &mut zip::ZipArchive<std::fs::File>,
    zip_path: &Path,
) -> Result<Option<String>> {
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(PackError::Provider(format!(
            "zip archive '{}' has {} entries; limit is {MAX_ZIP_ENTRIES}",
            zip_path.display(),
            archive.len()
        )));
    }

    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|e| {
            PackError::Parse(format!(
                "read entry {index} from '{}': {e}",
                zip_path.display()
            ))
        })?;
        let name = entry_name(&entry, zip_path)?;
        if is_symlink_entry(&entry) {
            return Err(PackError::UnsafePath(PathBuf::from(name)));
        }
        let rel = normalize_relative(Path::new(&name))?;
        entries.push((name, rel, entry.is_dir(), entry.size()));
    }

    let file_names: Vec<PathBuf> = entries
        .iter()
        .filter(|(_, _, is_dir, _)| !is_dir)
        .map(|(_, rel, _, _)| rel.clone())
        .collect();
    let wrapper = common_wrapper(&file_names);

    let mut total_uncompressed = 0u64;
    let mut paths = std::collections::HashSet::with_capacity(entries.len());
    for (name, rel, is_dir, size) in entries {
        let Some(rel) = strip_wrapper(rel, wrapper.as_deref()) else {
            continue;
        };
        // Only files can collide: a directory entry (a stray top-level
        // `mods/` next to a wrapper's `Server/mods/`, say) just creates a
        // directory that may already exist.
        if !is_dir && !paths.insert(rel) {
            return Err(PackError::Provider(format!(
                "zip archive '{}' contains a duplicate path '{name}'",
                zip_path.display(),
            )));
        }
        if !is_dir {
            if size > MAX_ENTRY_UNCOMPRESSED_BYTES {
                return Err(PackError::Provider(format!(
                    "zip archive '{}' contains an entry larger than {MAX_ENTRY_UNCOMPRESSED_BYTES} bytes",
                    zip_path.display()
                )));
            }
            total_uncompressed = total_uncompressed
                .checked_add(size)
                .ok_or_else(|| PackError::Provider("zip total size overflow".to_string()))?;
            if total_uncompressed > MAX_TOTAL_UNCOMPRESSED_BYTES {
                return Err(PackError::Provider(format!(
                    "zip archive '{}' exceeds the {MAX_TOTAL_UNCOMPRESSED_BYTES}-byte extraction limit",
                    zip_path.display()
                )));
            }
        }
    }
    Ok(wrapper)
}

/// Decodes a zip entry's name. A name that is not valid in its declared
/// encoding is reported as a parse error instead of being guessed at, so a
/// path check never runs on a lossy rendering of the real name.
pub(crate) fn entry_name<R: Read + ?Sized>(
    entry: &zip::read::ZipFile<'_, R>,
    zip_path: &Path,
) -> Result<String> {
    entry.name().map(|name| name.into_owned()).map_err(|e| {
        PackError::Parse(format!(
            "invalid entry name in '{}': {e}",
            zip_path.display()
        ))
    })
}

const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

fn is_symlink_entry<R: Read + ?Sized>(entry: &zip::read::ZipFile<'_, R>) -> bool {
    entry
        .unix_mode()
        .is_some_and(|mode| mode & S_IFMT == S_IFLNK)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::fs::hashing::sha256_bytes;

    fn reserved() -> ReservedPaths {
        ReservedPaths::default_layout()
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
        let files = extract_server_pack(&zip_path, &dest, &reserved())
            .unwrap()
            .files;

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
    fn extract_server_pack_accepts_directory_entries() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("dirs.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .add_directory("mods/", SimpleFileOptions::default())
                .unwrap();
            writer
                .add_directory("config/", SimpleFileOptions::default())
                .unwrap();
            writer
                .start_file("config/x.toml", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"x").unwrap();
            writer.finish().unwrap();
        }

        let dest = dir.path().join("server");
        let files = extract_server_pack(&zip_path, &dest, &reserved())
            .unwrap()
            .files;

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rel_path, PathBuf::from("config/x.toml"));
        assert_eq!(
            std::fs::read_to_string(dest.join("config/x.toml")).unwrap(),
            "x"
        );
    }

    #[test]
    fn extract_server_pack_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("evil.zip");
        write_zip(&zip_path, &[("mods/a.jar", b"aaa"), ("../evil", b"boom")]);

        let dest = dir.path().join("server");
        let result = extract_server_pack(&zip_path, &dest, &reserved());
        assert!(matches!(result, Err(PackError::UnsafePathComponent { .. })));
        assert!(!dir.path().join("evil").exists());
        assert!(!dest.join("evil").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extract_server_pack_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("abs.zip");
        write_zip(&zip_path, &[("/etc/evil", b"boom")]);

        let dest = dir.path().join("server");
        let result = extract_server_pack(&zip_path, &dest, &reserved());
        assert!(matches!(result, Err(PackError::UnsafePath(_))));
    }

    #[test]
    fn extract_server_pack_rejects_backslash_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("bs.zip");
        write_zip(&zip_path, &[("..\\evil", b"boom")]);

        let dest = dir.path().join("server");
        let result = extract_server_pack(&zip_path, &dest, &reserved());
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
        let result = extract_server_pack(&zip_path, &dest, &reserved());
        assert!(matches!(result, Err(PackError::UnsafePath(_))));
    }

    #[test]
    fn extract_server_pack_rejects_duplicate_normalized_paths() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("duplicate.zip");
        write_zip(
            &zip_path,
            &[("mods/a.jar", b"one"), ("mods\\a.jar", b"two")],
        );

        let result = extract_server_pack(&zip_path, &dir.path().join("server"), &reserved());
        assert!(
            matches!(result, Err(PackError::Provider(message)) if message.contains("duplicate path"))
        );
    }

    #[test]
    fn extract_server_pack_rejects_too_many_entries() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("many.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for index in 0..=MAX_ZIP_ENTRIES {
            writer
                .start_file(format!("files/{index}"), SimpleFileOptions::default())
                .unwrap();
        }
        writer.finish().unwrap();

        let result = extract_server_pack(&zip_path, &dir.path().join("server"), &reserved());
        assert!(
            matches!(result, Err(PackError::Provider(message)) if message.contains("entries; limit"))
        );
    }

    fn sorted_rels(files: &[PreparedFile]) -> Vec<PathBuf> {
        let mut rels: Vec<PathBuf> = files.iter().map(|f| f.rel_path.clone()).collect();
        rels.sort();
        rels
    }

    #[test]
    fn strips_single_wrapper_dir_containing_mods() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("wrapped.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .add_directory("ServerFiles-1.2/", SimpleFileOptions::default())
                .unwrap();
            for (name, data) in [
                ("ServerFiles-1.2/mods/a.jar", b"a".as_slice()),
                ("ServerFiles-1.2/config/x.toml", b"x".as_slice()),
                ("ServerFiles-1.2/startserver.sh", b"run".as_slice()),
            ] {
                writer
                    .start_file(name, SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }

        let dest = dir.path().join("server");
        let extracted = extract_server_pack(&zip_path, &dest, &reserved()).unwrap();

        assert_eq!(extracted.wrapper.as_deref(), Some("ServerFiles-1.2"));
        assert_eq!(
            sorted_rels(&extracted.files),
            vec![
                PathBuf::from("config/x.toml"),
                PathBuf::from("mods/a.jar"),
                PathBuf::from("startserver.sh"),
            ]
        );
        assert_eq!(std::fs::read(dest.join("mods/a.jar")).unwrap(), b"a");
        assert!(!dest.join("ServerFiles-1.2").exists());
        let notices = extracted.notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("'ServerFiles-1.2/'"), "{notices:?}");
    }

    #[test]
    fn keeps_layout_without_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("plain.zip");
        write_zip(
            &zip_path,
            &[
                ("mods/a.jar", b"a"),
                ("config/x.toml", b"x"),
                ("run.sh", b"r"),
            ],
        );

        let extracted =
            extract_server_pack(&zip_path, &dir.path().join("server"), &reserved()).unwrap();

        assert_eq!(extracted.wrapper, None);
        assert!(extracted.notices().is_empty());
        assert_eq!(
            sorted_rels(&extracted.files),
            vec![
                PathBuf::from("config/x.toml"),
                PathBuf::from("mods/a.jar"),
                PathBuf::from("run.sh"),
            ]
        );
    }

    #[test]
    fn wrapper_without_known_dirs_not_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("docs.zip");
        write_zip(
            &zip_path,
            &[("docs/readme.txt", b"r"), ("docs/guide/intro.md", b"i")],
        );

        let extracted =
            extract_server_pack(&zip_path, &dir.path().join("server"), &reserved()).unwrap();

        assert_eq!(extracted.wrapper, None);
        assert_eq!(
            sorted_rels(&extracted.files),
            vec![
                PathBuf::from("docs/guide/intro.md"),
                PathBuf::from("docs/readme.txt"),
            ]
        );
    }

    #[test]
    fn config_only_zip_with_nested_pack_dir_is_not_rerooted() {
        // config/kubejs/... must stay under config/, not become kubejs/...
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("config-only.zip");
        write_zip(
            &zip_path,
            &[
                ("config/kubejs/common.js", b"js"),
                ("config/mods/settings.toml", b"s"),
                ("config/server.toml", b"t"),
            ],
        );

        let extracted =
            extract_server_pack(&zip_path, &dir.path().join("server"), &reserved()).unwrap();

        assert_eq!(extracted.wrapper, None);
        assert_eq!(
            sorted_rels(&extracted.files),
            vec![
                PathBuf::from("config/kubejs/common.js"),
                PathBuf::from("config/mods/settings.toml"),
                PathBuf::from("config/server.toml"),
            ]
        );
    }

    #[test]
    fn stray_top_level_directory_entry_next_to_a_wrapper_is_not_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("stray.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            for name in ["mods/", "Server/", "Server/mods/"] {
                writer
                    .add_directory(name, SimpleFileOptions::default())
                    .unwrap();
            }
            for (name, data) in [
                ("Server/mods/a.jar", b"a".as_slice()),
                ("Server/eula.txt", b"eula=true".as_slice()),
            ] {
                writer
                    .start_file(name, SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }

        let dest = dir.path().join("server");
        let extracted = extract_server_pack(&zip_path, &dest, &reserved()).unwrap();
        assert_eq!(extracted.wrapper.as_deref(), Some("Server"));
        assert_eq!(
            sorted_rels(&extracted.files),
            vec![PathBuf::from("eula.txt"), PathBuf::from("mods/a.jar")]
        );
        assert_eq!(std::fs::read(dest.join("mods/a.jar")).unwrap(), b"a");
    }

    #[test]
    fn duplicates_are_detected_after_stripping() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("dup.zip");
        write_zip(
            &zip_path,
            &[("W/mods/a.jar", b"one"), ("W\\mods\\a.jar", b"two")],
        );

        let result = extract_server_pack(&zip_path, &dir.path().join("server"), &reserved());
        assert!(
            matches!(result, Err(PackError::Provider(message)) if message.contains("duplicate path"))
        );
    }

    #[test]
    fn common_wrapper_rules() {
        let names = |list: &[&str]| list.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(
            common_wrapper(&names(&["W/mods/a.jar", "W/eula.txt"])),
            Some("W".to_string())
        );
        // A marker directory needs content under it.
        assert_eq!(common_wrapper(&names(&["W/mods", "W/eula.txt"])), None);
        // Two top-level folders: no wrapper.
        assert_eq!(
            common_wrapper(&names(&["W/mods/a.jar", "X/config/b.toml"])),
            None
        );
        // A top-level file: no wrapper.
        assert_eq!(common_wrapper(&names(&["W/mods/a.jar", "eula.txt"])), None);
        // A pack directory is never a wrapper.
        assert_eq!(common_wrapper(&names(&["mods/config/a.toml"])), None);
        assert_eq!(common_wrapper(&[]), None);
    }
    const LAYERS: &[&str] = &["overrides", "server-overrides"];

    #[test]
    fn prefixed_layers_server_overrides_win() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.mrpack");
        write_zip(
            &zip_path,
            &[
                ("modrinth.index.json", b"{}"),
                ("overrides/config/a.toml", b"overrides"),
                ("overrides/config/only.toml", b"only"),
                ("server-overrides/config/a.toml", b"server"),
                ("server-overrides/mods/s.jar", b"s"),
            ],
        );
        let dest = dir.path().join("server");
        let written = extract_prefixed_layers(&zip_path, &dest, LAYERS, &reserved())
            .unwrap()
            .written;
        assert_eq!(
            written.into_iter().collect::<Vec<_>>(),
            vec![
                PathBuf::from("config/a.toml"),
                PathBuf::from("config/only.toml"),
                PathBuf::from("mods/s.jar"),
            ]
        );
        assert_eq!(
            std::fs::read(dest.join("config/a.toml")).unwrap(),
            b"server"
        );
        assert_eq!(
            std::fs::read(dest.join("config/only.toml")).unwrap(),
            b"only"
        );
        // Entries outside the layers are not extracted.
        assert!(!dest.join("modrinth.index.json").exists());
    }

    #[test]
    fn client_overrides_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.mrpack");
        write_zip(
            &zip_path,
            &[
                ("client-overrides/options.txt", b"client"),
                ("client-overrides/config/a.toml", b"client"),
                ("overrides/config/a.toml", b"common"),
            ],
        );
        let dest = dir.path().join("server");
        let written = extract_prefixed_layers(&zip_path, &dest, LAYERS, &reserved())
            .unwrap()
            .written;
        assert_eq!(written.len(), 1);
        assert_eq!(
            std::fs::read(dest.join("config/a.toml")).unwrap(),
            b"common"
        );
        assert!(!dest.join("options.txt").exists());
    }

    #[test]
    fn duplicate_within_layer_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.mrpack");
        // Distinct entry names that normalize to the same path.
        write_zip(
            &zip_path,
            &[
                ("overrides/config/a.toml", b"one"),
                ("overrides\\config\\a.toml", b"two"),
            ],
        );
        let dest = dir.path().join("server");
        let result = extract_prefixed_layers(&zip_path, &dest, LAYERS, &reserved());
        assert!(
            matches!(&result, Err(PackError::Provider(m)) if m.contains("duplicate path")),
            "{result:?}"
        );
        assert!(!dest.join("config").exists());
    }

    #[test]
    fn traversal_in_override_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.mrpack");
        write_zip(
            &zip_path,
            &[
                ("overrides/config/a.toml", b"ok"),
                ("overrides/../../evil.txt", b"boom"),
            ],
        );
        let dest = dir.path().join("server");
        let result = extract_prefixed_layers(&zip_path, &dest, LAYERS, &reserved());
        assert!(
            matches!(result, Err(PackError::UnsafePathComponent { .. })),
            "{result:?}"
        );
        assert!(!dir.path().join("evil.txt").exists());
        assert!(!dest.join("config/a.toml").exists());
    }

    #[test]
    fn control_files_in_layers_rejected() {
        for name in [
            "overrides/.packctl/state.json",
            "server-overrides/.packctl.toml",
            "overrides/.PackctlIgnore",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let zip_path = dir.path().join("pack.mrpack");
            write_zip(&zip_path, &[(name, b"evil")]);
            let dest = dir.path().join("server");
            let result = extract_prefixed_layers(&zip_path, &dest, LAYERS, &reserved());
            assert!(matches!(result, Err(PackError::UnsafePath(_))), "{name}");
            assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0, "{name}");
        }
    }

    #[test]
    fn prepared_files_for_hashes_given_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mods")).unwrap();
        std::fs::write(dir.path().join("mods/a.jar"), b"abc").unwrap();
        let files = prepared_files_for(dir.path(), [PathBuf::from("mods/a.jar")]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].size, 3);
        assert_eq!(files[0].sha256, sha256_bytes(b"abc"));
        assert!(prepared_files_for(dir.path(), [PathBuf::from("../x")]).is_err());
    }

    /// A profile whose overlay is `config/my-overlay` and whose archive is
    /// `drops/pack.zip`, both inside the server root.
    fn custom_reserved() -> ReservedPaths {
        ReservedPaths::control_files_and(&["config/my-overlay", "drops/pack.zip"])
    }

    #[test]
    fn server_pack_skips_reserved_paths_with_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.zip");
        write_zip(
            &zip_path,
            &[
                ("mods/a.jar", b"a"),
                (".packctl.toml", b"evil"),
                (".packctl/snapshots/x/manifest.json", b"evil"),
                ("config/my-overlay/mods/x.jar", b"evil"),
                ("drops/pack.zip", b"evil"),
            ],
        );
        let dest = dir.path().join("server");
        let extracted = extract_server_pack(&zip_path, &dest, &custom_reserved()).unwrap();

        assert_eq!(
            sorted_rels(&extracted.files),
            vec![PathBuf::from("mods/a.jar")]
        );
        assert_eq!(
            extracted.reserved_skipped,
            vec![
                ".packctl.toml",
                ".packctl/snapshots/x/manifest.json",
                "config/my-overlay/mods/x.jar",
                "drops/pack.zip",
            ]
        );
        assert!(!dest.join(".packctl.toml").exists());
        assert!(!dest.join(".packctl").exists());
        assert!(!dest.join("config").exists());
        assert!(!dest.join("drops").exists());
        let notices = extracted.notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with("Skipped 4 pack file(s) in locations packctl reserves"),
            "{notices:?}"
        );
    }

    #[test]
    fn layers_skip_profile_reserved_paths() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.mrpack");
        write_zip(
            &zip_path,
            &[
                ("overrides/config/a.toml", b"ok"),
                ("overrides/config/my-overlay/mods/x.jar", b"evil"),
                ("server-overrides/drops/pack.zip", b"evil"),
            ],
        );
        let dest = dir.path().join("server");
        let layers = extract_prefixed_layers(&zip_path, &dest, LAYERS, &custom_reserved()).unwrap();
        assert_eq!(
            layers.written.into_iter().collect::<Vec<_>>(),
            vec![PathBuf::from("config/a.toml")]
        );
        assert_eq!(
            layers.reserved_skipped,
            vec!["config/my-overlay/mods/x.jar", "drops/pack.zip"]
        );
        assert!(!dest.join("config/my-overlay").exists());
        assert!(!dest.join("drops").exists());
    }
}
