//! SHA-256 hashing helpers for content comparison.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{PackError, Result};

/// Lowercase hex-encoded SHA-256 digest of `data`.
pub fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    to_hex(&hasher.finalize())
}

/// Lowercase hex-encoded SHA-256 digest of the file at `path`.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .map_err(|e| PackError::io(format!("open '{}' for hashing", path.display()), e))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|e| PackError::io(format!("read '{}' while hashing", path.display()), e))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// Hashes many files using at most `concurrency` worker threads.
///
/// Input order is preserved in the returned vector. A `concurrency` of zero
/// falls back to a single worker. This is an intentionally synchronous
/// CPU/IO-bound helper used outside the async core.
pub fn hash_files_parallel(
    paths: &[PathBuf],
    concurrency: usize,
) -> Result<Vec<(PathBuf, String)>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let concurrency = concurrency.max(1);
    let num_threads = concurrency.min(paths.len());
    let chunk_size = paths.len().div_ceil(num_threads);

    std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|path| sha256_file(path).map(|hash| (path.clone(), hash)))
                        .collect::<Result<Vec<_>>>()
                })
            })
            .collect();

        let mut results = Vec::with_capacity(paths.len());
        for handle in handles {
            let chunk_results = handle
                .join()
                .map_err(|_| PackError::Other("hashing thread panicked".to_string()))??;
            results.extend(chunk_results);
        }
        Ok(results)
    })
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC_HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn sha256_bytes_known_vectors() {
        assert_eq!(sha256_bytes(b""), EMPTY_HASH);
        assert_eq!(sha256_bytes(b"abc"), ABC_HASH);
    }

    #[test]
    fn sha256_file_matches_sha256_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), ABC_HASH);
        assert_eq!(sha256_file(&path).unwrap(), sha256_bytes(b"abc"));
    }

    #[test]
    fn hash_files_parallel_returns_ordered_results() {
        let dir = tempfile::tempdir().unwrap();
        let contents = [
            b"abc".to_vec(),
            b"".to_vec(),
            b"hello world".to_vec(),
            b"def".to_vec(),
            b"xyz".to_vec(),
        ];
        let paths: Vec<PathBuf> = (0..contents.len())
            .map(|i| {
                let path = dir.path().join(format!("file_{i}.bin"));
                std::fs::write(&path, &contents[i]).unwrap();
                path
            })
            .collect();

        let expected: Vec<(PathBuf, String)> = paths
            .iter()
            .zip(&contents)
            .map(|(path, data)| (path.clone(), sha256_bytes(data)))
            .collect();

        for concurrency in [0, 1, 4] {
            let results = hash_files_parallel(&paths, concurrency).unwrap();
            assert_eq!(results, expected, "concurrency = {concurrency}");
        }
    }

    #[test]
    fn hash_files_parallel_empty_input() {
        assert!(hash_files_parallel(&[], 4).unwrap().is_empty());
    }
}
