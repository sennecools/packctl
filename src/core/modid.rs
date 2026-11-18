//! Mod identity of jar files.
//!
//! Two jars that carry the same mod id cannot both be loaded: the server
//! refuses to start. `packctl adopt` uses mod ids to recognise a pack mod the
//! admin replaced with another version, and the updater uses them to warn
//! when upstream, the overlay and kept files would put two copies of one mod
//! into `mods/`.
//!
//! The id comes from the jar's loader metadata (Forge and NeoForge
//! `mods.toml`, Fabric `fabric.mod.json`, Quilt `quilt.mod.json`). Only those
//! small entries are read. A jar without readable metadata falls back to a
//! key derived from its file name.

use std::io::Read;
use std::path::Path;

use crate::error::{PackError, Result};
use crate::providers::sanitize;

/// Largest metadata entry read from a jar.
const MAX_METADATA_BYTES: u64 = 1024 * 1024;

/// Metadata entries that name a jar's mods.
const NEOFORGE_MODS_TOML: &str = "META-INF/neoforge.mods.toml";
const FORGE_MODS_TOML: &str = "META-INF/mods.toml";
const FABRIC_MOD_JSON: &str = "fabric.mod.json";
const QUILT_MOD_JSON: &str = "quilt.mod.json";

/// The mod ids a jar declares in its loader metadata, in declaration order
/// and without duplicates.
///
/// A jar that cannot be opened is an error. A file that is not a zip, or
/// whose metadata is missing, oversized or unparsable, yields an empty list:
/// metadata is a hint, never a reason to fail.
pub fn jar_mod_ids(path: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(path)
        .map_err(|error| PackError::io(format!("open jar '{}'", path.display()), error))?;
    let Ok(mut archive) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for (entry, parse) in [
        (NEOFORGE_MODS_TOML, toml_mod_ids as fn(&str) -> Vec<String>),
        (FORGE_MODS_TOML, toml_mod_ids),
        (FABRIC_MOD_JSON, fabric_mod_id),
        (QUILT_MOD_JSON, quilt_mod_id),
    ] {
        if let Some(text) = read_small_entry(&mut archive, entry) {
            for id in parse(&text) {
                let id = sanitize(id.trim());
                if is_plausible_id(&id) && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    Ok(ids)
}

/// Reads `name` from the archive as UTF-8 when it exists and is small.
fn read_small_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let entry = archive.by_name(name).ok()?;
    if entry.size() > MAX_METADATA_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    entry
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// A metadata id worth comparing: not empty and not an unexpanded build
/// placeholder such as `${mod_id}`.
fn is_plausible_id(id: &str) -> bool {
    !id.is_empty() && !id.contains('$') && !id.contains(char::is_whitespace)
}

/// `[[mods]] modId = "..."` from a Forge or NeoForge `mods.toml`.
fn toml_mod_ids(text: &str) -> Vec<String> {
    let Ok(value) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    value
        .get("mods")
        .and_then(toml::Value::as_array)
        .map(|mods| {
            mods.iter()
                .filter_map(|module| module.get("modId")?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `"id"` from a `fabric.mod.json`.
fn fabric_mod_id(text: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| value.get("id")?.as_str().map(str::to_string))
        .into_iter()
        .collect()
}

/// `"quilt_loader": { "id": ... }` from a `quilt.mod.json`.
fn quilt_mod_id(text: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("quilt_loader")?
                .get("id")?
                .as_str()
                .map(str::to_string)
        })
        .into_iter()
        .collect()
}

/// Tokens of a jar file name that say nothing about which mod it is.
const NOISE_TOKENS: &[&str] = &[
    "forge",
    "neoforge",
    "fabric",
    "quilt",
    "mc",
    "universal",
    "server",
    "all",
];

/// A key naming the mod a jar file holds, from its file name alone.
///
/// The name is lowercased, its `.jar` and `.disabled` suffixes are removed,
/// and it is split on `-`, `_`, `+` and spaces. Version-like tokens (those
/// starting with a digit, `mc1.20.1`, `v2`) and loader or platform words are
/// dropped, and the rest is joined with `-`. `create-1.20.1-0.5.1.jar` and
/// `create-1.20.1-0.5.2.jar` share the key `create`; `create_new_age-1.1.jar`
/// is `create-new-age`.
pub fn filename_mod_key(name: &str) -> String {
    let mut base = name.trim().to_lowercase();
    loop {
        if let Some(stripped) = base.strip_suffix(".disabled") {
            base = stripped.to_string();
        } else if let Some(stripped) = base.strip_suffix(".jar") {
            base = stripped.to_string();
        } else {
            break;
        }
    }
    base.split(['-', '_', '+', ' '])
        .filter(|token| !token.is_empty() && !is_noise_token(token))
        .collect::<Vec<_>>()
        .join("-")
}

fn is_noise_token(token: &str) -> bool {
    let starts_with_digit = |text: &str| text.chars().next().is_some_and(|c| c.is_ascii_digit());
    starts_with_digit(token)
        || NOISE_TOKENS.contains(&token)
        || token.strip_prefix("mc").is_some_and(starts_with_digit)
        || token.strip_prefix('v').is_some_and(starts_with_digit)
}

/// The identity used to pair jars: the metadata mod ids, or else the file
/// name key. Empty when neither yields anything.
pub fn mod_identity(path: &Path) -> Vec<String> {
    let ids = jar_mod_ids(path).unwrap_or_default();
    if !ids.is_empty() {
        return ids;
    }
    let key = path
        .file_name()
        .map(|name| filename_mod_key(&name.to_string_lossy()))
        .unwrap_or_default();
    if key.is_empty() {
        Vec::new()
    } else {
        vec![key]
    }
}

/// Test helper: writes a jar (zip) with the given entries.
#[cfg(test)]
pub(crate) fn write_jar(path: &Path, entries: &[(&str, &[u8])]) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, data) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap();
}

/// Test helper: a NeoForge jar declaring `mod_id`, with `filler` making its
/// content unique.
#[cfg(test)]
pub(crate) fn write_mod_jar(path: &Path, mod_id: &str, filler: &str) {
    let toml = format!("modLoader=\"javafml\"\n[[mods]]\nmodId=\"{mod_id}\"\nversion=\"1\"\n");
    write_jar(
        path,
        &[
            (NEOFORGE_MODS_TOML, toml.as_bytes()),
            ("filler.txt", filler.as_bytes()),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_neoforge_mods_toml() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("create.jar");
        write_jar(
            &jar,
            &[(
                NEOFORGE_MODS_TOML,
                b"modLoader=\"javafml\"\nloaderVersion=\"[4,)\"\n[[mods]]\nmodId=\"create\"\n[[mods]]\nmodId=\"ponder\"\n[[dependencies.create]]\nmodId=\"neoforge\"\n",
            )],
        );
        assert_eq!(jar_mod_ids(&jar).unwrap(), vec!["create", "ponder"]);

        // The older Forge file name works too.
        let forge = dir.path().join("jei.jar");
        write_jar(&forge, &[(FORGE_MODS_TOML, b"[[mods]]\nmodId=\"jei\"\n")]);
        assert_eq!(jar_mod_ids(&forge).unwrap(), vec!["jei"]);
    }

    #[test]
    fn reads_fabric_json() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("sodium.jar");
        write_jar(
            &jar,
            &[(
                FABRIC_MOD_JSON,
                br#"{"schemaVersion": 1, "id": "sodium", "version": "0.5"}"#,
            )],
        );
        assert_eq!(jar_mod_ids(&jar).unwrap(), vec!["sodium"]);
    }

    #[test]
    fn reads_quilt_json() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("qsl.jar");
        write_jar(
            &jar,
            &[(
                QUILT_MOD_JSON,
                br#"{"schema_version": 1, "quilt_loader": {"group": "org.x", "id": "qsl"}}"#,
            )],
        );
        assert_eq!(jar_mod_ids(&jar).unwrap(), vec!["qsl"]);
    }

    #[test]
    fn garbage_jar_yields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let not_zip = dir.path().join("broken.jar");
        std::fs::write(&not_zip, b"definitely not a zip").unwrap();
        assert!(jar_mod_ids(&not_zip).unwrap().is_empty());

        let bad_toml = dir.path().join("bad.jar");
        write_jar(&bad_toml, &[(NEOFORGE_MODS_TOML, b"[[mods]\nmodId=")]);
        assert!(jar_mod_ids(&bad_toml).unwrap().is_empty());

        let template = dir.path().join("dev.jar");
        write_jar(
            &template,
            &[(FORGE_MODS_TOML, b"[[mods]]\nmodId=\"${mod_id}\"\n")],
        );
        assert!(jar_mod_ids(&template).unwrap().is_empty());

        let oversized = dir.path().join("huge.jar");
        let mut big = b"[[mods]]\nmodId=\"huge\"\n".to_vec();
        big.resize(MAX_METADATA_BYTES as usize + 10, b'\n');
        write_jar(&oversized, &[(NEOFORGE_MODS_TOML, &big)]);
        assert!(jar_mod_ids(&oversized).unwrap().is_empty());

        // A missing file is an I/O error, not an empty list.
        assert!(jar_mod_ids(&dir.path().join("missing.jar")).is_err());
        // mod_identity falls back to the file name.
        assert_eq!(mod_identity(&not_zip), vec!["broken"]);
    }

    #[test]
    fn filename_key_strips_versions_and_loaders() {
        assert_eq!(filename_mod_key("create-1.20.1-0.5.1.f.jar"), "create");
        assert_eq!(filename_mod_key("jei-1.20.1-forge-15.2.0.27.jar"), "jei");
        assert_eq!(filename_mod_key("JEI-1.21.1-neoforge-19.0.jar"), "jei");
        assert_eq!(
            filename_mod_key("sophisticatedbackpacks-mc1.20.1-3.20.2.jar"),
            "sophisticatedbackpacks"
        );
        assert_eq!(
            filename_mod_key("ftb-library-fabric+2001.1.3.jar"),
            "ftb-library"
        );
        assert_eq!(
            filename_mod_key("appleskin-v2.5.1.jar.disabled"),
            "appleskin"
        );
        assert_eq!(filename_mod_key("Mod Name_universal.jar"), "mod-name");
        assert_eq!(filename_mod_key("1.2.3.jar"), "");
    }

    #[test]
    fn create_vs_create_new_age_differ() {
        assert_ne!(
            filename_mod_key("create-1.20.1-0.5.1.jar"),
            filename_mod_key("create_new_age-1.20.1-1.1.2.jar")
        );
        assert_eq!(
            filename_mod_key("create_new_age-1.20.1-1.1.2.jar"),
            "create-new-age"
        );
    }
}
