//! Safe zip extraction shared by pack providers.
//!
//! Archive entry paths are treated as untrusted: absolute paths, `..`
//! components, `.`/empty components, NUL bytes, backslash traversal, symlink
//! entries, duplicate normalized paths, and archives that exceed hard size or
//! entry-count limits abort the whole extraction with a [`crate::error::PackError`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{PackError, Result};
use crate::fs::hashing::sha256_file;
use crate::fs::paths::{normalize_relative, safe_join};
use crate::providers::PreparedFile;

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

    validate_archive(&mut archive, zip_path)?;
    let mut dest_paths = Vec::new();
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
            total_uncompressed = total_uncompressed
                .checked_add(read as u64)
                .ok_or_else(|| PackError::Provider("zip total size overflow".to_string()))?;
            if entry_uncompressed > MAX_ENTRY_UNCOMPRESSED_BYTES
                || total_uncompressed > MAX_TOTAL_UNCOMPRESSED_BYTES
            {
                return Err(PackError::Provider(
                    "zip archive exceeds extraction size limits".to_string(),
                ));
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

const MAX_ZIP_ENTRIES: usize = 10_000;
const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ENTRY_UNCOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;

fn validate_archive(archive: &mut zip::ZipArchive<std::fs::File>, zip_path: &Path) -> Result<()> {
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(PackError::Provider(format!(
            "zip archive '{}' has {} entries; limit is {MAX_ZIP_ENTRIES}",
            zip_path.display(),
            archive.len()
        )));
    }
    let mut total_uncompressed = 0u64;
    let mut paths = std::collections::HashSet::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|e| {
            PackError::Parse(format!(
                "read entry {index} from '{}': {e}",
                zip_path.display()
            ))
        })?;
        if is_symlink_entry(&entry) {
            return Err(PackError::UnsafePath(PathBuf::from(entry.name())));
        }
        let rel = normalize_relative(Path::new(entry.name()))?;
        if !paths.insert(rel) {
            return Err(PackError::Provider(format!(
                "zip archive '{}' contains a duplicate path '{}'",
                zip_path.display(),
                entry.name()
            )));
        }
        if !entry.is_dir() {
            if entry.size() > MAX_ENTRY_UNCOMPRESSED_BYTES {
                return Err(PackError::Provider(format!(
                    "zip archive '{}' contains an entry larger than {MAX_ENTRY_UNCOMPRESSED_BYTES} bytes",
                    zip_path.display()
                )));
            }
            total_uncompressed = total_uncompressed
                .checked_add(entry.size())
                .ok_or_else(|| PackError::Provider("zip total size overflow".to_string()))?;
            if total_uncompressed > MAX_TOTAL_UNCOMPRESSED_BYTES {
                return Err(PackError::Provider(format!(
                    "zip archive '{}' exceeds the {MAX_TOTAL_UNCOMPRESSED_BYTES}-byte extraction limit",
                    zip_path.display()
                )));
            }
        }
    }
    Ok(())
}

const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

fn is_symlink_entry(entry: &zip::read::ZipFile<'_>) -> bool {
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
    fn extract_server_pack_rejects_duplicate_normalized_paths() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("duplicate.zip");
        write_zip(
            &zip_path,
            &[("mods/a.jar", b"one"), ("mods\\a.jar", b"two")],
        );

        let result = extract_server_pack(&zip_path, &dir.path().join("server"));
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

        let result = extract_server_pack(&zip_path, &dir.path().join("server"));
        assert!(
            matches!(result, Err(PackError::Provider(message)) if message.contains("entries; limit"))
        );
    }
}
