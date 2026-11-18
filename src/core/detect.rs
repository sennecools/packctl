//! Guessing which pack version a hand-installed server runs.
//!
//! `packctl adopt --detect` compares the jars in the server's `mods/` folder
//! with the jars each candidate pack version ships. A provider supplies the
//! candidates' jar fingerprints (see
//! [`crate::providers::PackProvider::version_fingerprints`]); a version is
//! only picked when it clearly matches better than every other candidate.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::error::{PackError, Result};
use crate::providers::PackVersion;

/// Most candidate versions fingerprinted, newest first.
pub const MAX_CANDIDATES: usize = 15;

/// Lowest score a version needs to be picked automatically.
pub const PICK_THRESHOLD: f64 = 0.85;

/// How far the best version must lead the runner-up to be picked.
pub const PICK_MARGIN: f64 = 0.05;

/// How many candidates an ambiguous detection lists.
const AMBIGUOUS_LISTED: usize = 3;

/// The fingerprint of one jar in `mods/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JarPrint {
    /// The jar's file name (without the `mods/` folder).
    pub file_name: String,
    pub size: u64,
    pub crc32: Option<u32>,
    /// Lowercase hex SHA-1.
    pub sha1: Option<String>,
}

/// The jars one pack version ships in `mods/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionFingerprint {
    pub version: PackVersion,
    pub jars: Vec<JarPrint>,
}

/// The outcome of version detection.
#[derive(Debug, Clone, PartialEq)]
pub enum Detection {
    /// One version matches clearly; `score` is the fraction of its jars found.
    Picked { version: PackVersion, score: f64 },
    /// No version matches clearly: the best candidates with their scores.
    Ambiguous(Vec<(PackVersion, f64)>),
}

/// The fraction of `candidate`'s jars found on the server: a live jar with
/// the same file name and size, and an equal CRC-32 or SHA-1 (whichever both
/// sides have; a jar with no comparable hash never matches). A candidate
/// without jars scores 0.
pub fn score(live: &[JarPrint], candidate: &VersionFingerprint) -> f64 {
    if candidate.jars.is_empty() {
        return 0.0;
    }
    let by_name: HashMap<&str, Vec<&JarPrint>> =
        live.iter().fold(HashMap::new(), |mut map, jar| {
            map.entry(jar.file_name.as_str()).or_default().push(jar);
            map
        });
    let found = candidate
        .jars
        .iter()
        .filter(|wanted| {
            by_name
                .get(wanted.file_name.as_str())
                .is_some_and(|jars| jars.iter().any(|jar| same_jar(jar, wanted)))
        })
        .count();
    found as f64 / candidate.jars.len() as f64
}

fn same_jar(live: &JarPrint, wanted: &JarPrint) -> bool {
    if live.size != wanted.size {
        return false;
    }
    match (live.crc32, wanted.crc32) {
        (Some(a), Some(b)) => a == b,
        _ => match (&live.sha1, &wanted.sha1) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        },
    }
}

/// Picks a version from scored candidates: the best one when it scores at
/// least [`PICK_THRESHOLD`] and leads the runner-up by at least
/// [`PICK_MARGIN`]; otherwise the top three, best first.
pub fn pick(mut scores: Vec<(PackVersion, f64)>) -> Detection {
    // A stable sort keeps the provider's newest-first order among equals.
    scores.sort_by(|a, b| b.1.total_cmp(&a.1));
    let runner_up = scores.get(1).map(|(_, score)| *score).unwrap_or(0.0);
    match scores.first() {
        Some((version, best)) if *best >= PICK_THRESHOLD && best - runner_up >= PICK_MARGIN => {
            Detection::Picked {
                version: version.clone(),
                score: *best,
            }
        }
        _ => {
            scores.truncate(AMBIGUOUS_LISTED);
            Detection::Ambiguous(scores)
        }
    }
}

/// Fingerprints the jars directly inside `<server_root>/mods`.
///
/// Symlinks and non-files are skipped. A jar is only hashed when some
/// candidate ships a jar with the same name and size, and only with the
/// hashes the candidates carry (`crc32` and/or `sha1`).
pub fn live_jar_prints(
    server_root: &Path,
    candidates: &[VersionFingerprint],
) -> Result<Vec<JarPrint>> {
    let mods = server_root.join("mods");
    let entries = match std::fs::read_dir(&mods) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PackError::io(format!("scan '{}'", mods.display()), error)),
    };
    let wanted: HashMap<(&str, u64), (bool, bool)> = candidates
        .iter()
        .flat_map(|candidate| &candidate.jars)
        .fold(HashMap::new(), |mut map, jar| {
            let need = map.entry((jar.file_name.as_str(), jar.size)).or_default();
            need.0 |= jar.crc32.is_some();
            need.1 |= jar.sha1.is_some();
            map
        });

    let mut prints = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| PackError::io(format!("scan '{}'", mods.display()), error))?;
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || !crate::fs::paths::is_jar_path(&path) {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let size = metadata.len();
        let (crc32, sha1) = match wanted.get(&(file_name.as_str(), size)) {
            Some(&(need_crc, need_sha1)) => jar_hashes(&path, need_crc, need_sha1)?,
            None => (None, None),
        };
        prints.push(JarPrint {
            file_name,
            size,
            crc32,
            sha1,
        });
    }
    prints.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(prints)
}

/// CRC-32 and/or SHA-1 of a file, in one read.
pub fn jar_hashes(path: &Path, crc: bool, sha1: bool) -> Result<(Option<u32>, Option<String>)> {
    if !crc && !sha1 {
        return Ok((None, None));
    }
    let mut file = std::fs::File::open(path)
        .map_err(|error| PackError::io(format!("open '{}'", path.display()), error))?;
    let mut crc_hasher = crc32fast::Hasher::new();
    let mut sha1_hasher = Sha1::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| PackError::io(format!("read '{}'", path.display()), error))?;
        if read == 0 {
            break;
        }
        if crc {
            crc_hasher.update(&buffer[..read]);
        }
        if sha1 {
            sha1_hasher.update(&buffer[..read]);
        }
    }
    Ok((
        crc.then(|| crc_hasher.finalize()),
        sha1.then(|| format!("{:x}", sha1_hasher.finalize())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(id: &str) -> PackVersion {
        PackVersion {
            id: id.to_string(),
            name: format!("Pack {id}"),
            ..Default::default()
        }
    }

    fn jar(name: &str, size: u64, crc: u32) -> JarPrint {
        JarPrint {
            file_name: name.to_string(),
            size,
            crc32: Some(crc),
            sha1: None,
        }
    }

    fn fingerprint(id: &str, jars: Vec<JarPrint>) -> VersionFingerprint {
        VersionFingerprint {
            version: version(id),
            jars,
        }
    }

    #[test]
    fn score_picks_exact_version() {
        let live = vec![
            jar("a.jar", 10, 1),
            jar("b.jar", 20, 2),
            jar("c.jar", 30, 3),
            jar("extra.jar", 5, 9),
        ];
        let v2 = fingerprint(
            "2",
            vec![
                jar("a.jar", 10, 1),
                jar("b.jar", 20, 2),
                jar("c.jar", 30, 3),
            ],
        );
        let v1 = fingerprint(
            "1",
            vec![
                jar("a.jar", 10, 1),
                jar("b.jar", 20, 7),
                jar("c.jar", 30, 3),
            ],
        );
        let v0 = fingerprint("0", vec![jar("old.jar", 1, 1)]);
        assert_eq!(score(&live, &v2), 1.0);
        assert!((score(&live, &v1) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(score(&live, &v0), 0.0);
        assert_eq!(score(&live, &fingerprint("e", Vec::new())), 0.0);

        let scores = vec![
            (v2.version.clone(), score(&live, &v2)),
            (v1.version.clone(), score(&live, &v1)),
            (v0.version.clone(), score(&live, &v0)),
        ];
        assert_eq!(
            pick(scores),
            Detection::Picked {
                version: version("2"),
                score: 1.0
            }
        );

        // SHA-1 is compared when CRC-32 is missing on either side.
        let by_sha = JarPrint {
            file_name: "a.jar".to_string(),
            size: 10,
            crc32: None,
            sha1: Some("ABC".to_string()),
        };
        let live_sha = JarPrint {
            sha1: Some("abc".to_string()),
            ..jar("a.jar", 10, 1)
        };
        assert_eq!(
            score(&[live_sha], &fingerprint("s", vec![by_sha.clone()])),
            1.0
        );
        // Neither hash comparable: no match.
        let bare = JarPrint {
            crc32: None,
            sha1: None,
            ..by_sha
        };
        assert_eq!(
            score(&[jar("a.jar", 10, 1)], &fingerprint("n", vec![bare])),
            0.0
        );
    }

    #[test]
    fn ambiguous_scores_require_explicit_version() {
        // Two versions that differ only in a jar the server lacks.
        let close = vec![
            (version("3"), 0.95),
            (version("2"), 0.93),
            (version("1"), 0.5),
            (version("0"), 0.1),
        ];
        match pick(close) {
            Detection::Ambiguous(top) => {
                assert_eq!(top.len(), 3);
                assert_eq!(top[0].0.id, "3");
                assert_eq!(top[1].0.id, "2");
            }
            other => panic!("expected ambiguity, got {other:?}"),
        }
        // A clear leader below the threshold is not picked either.
        assert!(matches!(
            pick(vec![(version("1"), 0.8), (version("0"), 0.1)]),
            Detection::Ambiguous(_)
        ));
        assert!(matches!(pick(Vec::new()), Detection::Ambiguous(ref v) if v.is_empty()));
        // A single candidate at the threshold is picked.
        assert!(matches!(
            pick(vec![(version("1"), 0.9)]),
            Detection::Picked { .. }
        ));
    }

    #[test]
    fn live_prints_hash_only_what_candidates_need() {
        let dir = tempfile::tempdir().unwrap();
        let mods = dir.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("a.jar"), b"aaaa").unwrap();
        std::fs::write(mods.join("b.jar"), b"bb").unwrap();
        std::fs::write(mods.join("notes.txt"), b"x").unwrap();

        let crc_a = crc32fast::hash(b"aaaa");
        let candidates = vec![fingerprint("1", vec![jar("a.jar", 4, crc_a)])];
        let prints = live_jar_prints(dir.path(), &candidates).unwrap();
        assert_eq!(prints.len(), 2);
        assert_eq!(prints[0].file_name, "a.jar");
        assert_eq!(prints[0].crc32, Some(crc_a));
        assert_eq!(prints[0].sha1, None);
        assert_eq!(prints[1].crc32, None, "b.jar is in no candidate");
        assert_eq!(score(&prints, &candidates[0]), 1.0);
    }
}
