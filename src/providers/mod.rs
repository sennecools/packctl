//! Pack providers.
//!
//! A provider resolves modpacks and versions and prepares a selected version
//! in staging. Core update logic must not know how any specific provider
//! works.

pub mod curseforge;
pub mod http;
pub mod local;
pub mod modrinth;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::profile::{DEFAULT_DROP_DIR, ProviderKind, ServerProfile};
use crate::core::detect::VersionFingerprint;
use crate::core::reserved::ReservedPaths;
use crate::error::{PackError, Result};

/// Replaces control characters (newlines, escapes) in provider-supplied text
/// with spaces before it reaches the terminal or a file packctl writes.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The upstream pack being followed.
///
/// `project_id` is the provider's own project identifier as text: a number
/// for CurseForge, a base62 id for Modrinth, and empty for local archives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRef {
    pub project_id: String,
    pub slug: String,
}

/// A single available version of a pack.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackVersion {
    /// Provider-internal version id. Never trust human-readable parsing.
    pub id: String,
    /// Human-readable version label, e.g. "4.12".
    pub name: String,
    /// The version number, when the provider keeps one apart from the name
    /// (Modrinth's `version_number`).
    pub number: Option<String>,
    /// Release date, if known.
    pub released: Option<String>,
    /// Minecraft versions the provider lists for this version.
    pub game_versions: Vec<String>,
    /// Mod loaders the provider lists for this version.
    pub loaders: Vec<String>,
}

/// A mod loader a pack version is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoaderKind {
    Forge,
    NeoForge,
    Fabric,
    Quilt,
}

impl fmt::Display for LoaderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LoaderKind::Forge => "Forge",
            LoaderKind::NeoForge => "NeoForge",
            LoaderKind::Fabric => "Fabric",
            LoaderKind::Quilt => "Quilt",
        })
    }
}

/// The loader (and its version) a pack version needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderRequirement {
    pub kind: LoaderKind,
    pub version: String,
}

/// What a prepared pack version needs from the server to run, when the
/// provider knows it. packctl reports these; it does not install loaders.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRequirements {
    pub minecraft: Option<String>,
    pub loader: Option<LoaderRequirement>,
}

impl PackRequirements {
    /// `Minecraft 1.21.1 · NeoForge 21.1.77`, leaving out what is unknown.
    /// Empty when nothing is known.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(minecraft) = &self.minecraft {
            parts.push(format!("Minecraft {minecraft}"));
        }
        if let Some(loader) = &self.loader {
            parts.push(format!("{} {}", loader.kind, loader.version));
        }
        parts.join(" · ")
    }
}

/// How the user selected a version.
#[derive(Debug, Clone)]
pub enum VersionSelector {
    /// Use the latest available version.
    Latest,
    /// Match by provider-internal id.
    Id(String),
    /// Match by human-readable name.
    Name(String),
    /// Free-form user input: an id, a name, or a unique token of a name such
    /// as `1.1.0` (see [`resolve_version_from_list`]).
    Query(String),
}

/// A version that has been resolved to a concrete provider file.
#[derive(Debug, Clone)]
pub struct ResolvedPackVersion {
    pub pack: PackRef,
    pub version: PackVersion,
}

/// A file known to belong to a prepared upstream pack.
#[derive(Debug, Clone)]
pub struct PreparedFile {
    /// Relative path inside the server root.
    pub rel_path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

/// A prepared (downloaded + extracted) upstream version in staging.
#[derive(Debug)]
pub struct PreparedPack {
    pub name: String,
    pub version: PackVersion,
    /// Root of the prepared server tree inside the staging directory.
    pub root: PathBuf,
    pub files: Vec<PreparedFile>,
    /// What the version needs from the server, when the provider knows it.
    pub requirements: Option<PackRequirements>,
    /// Informational notes from preparation (for example a stripped wrapper
    /// directory); the planner shows them as plan notices.
    pub notices: Vec<String>,
}

/// Resolves and prepares modpacks.
#[async_trait::async_trait]
pub trait PackProvider: Send + Sync {
    async fn list_versions(&self, pack: &PackRef) -> Result<Vec<PackVersion>>;
    async fn resolve_version(
        &self,
        pack: &PackRef,
        selector: &VersionSelector,
    ) -> Result<ResolvedPackVersion>;
    async fn prepare(&self, version: &ResolvedPackVersion, staging: &Path) -> Result<PreparedPack>;

    /// The jars each of `versions` ships in `mods/`, for guessing which
    /// version a hand-installed server runs (`packctl adopt --detect`).
    ///
    /// Providers that cannot fingerprint versions cheaply keep this default,
    /// which asks for an explicit version.
    async fn version_fingerprints(
        &self,
        _pack: &PackRef,
        _versions: &[PackVersion],
    ) -> Result<Vec<VersionFingerprint>> {
        Err(PackError::Provider(
            "automatic version detection is not supported for this provider; pass --as <version>"
                .into(),
        ))
    }
}

/// Builds the pack provider a profile's `[pack]` section describes.
///
/// CurseForge packs talk to the endpoint the profile resolves to: the
/// official API when an API key is configured (`$CF_API_KEY`, the profile's
/// stored key, or the shared global key), else the community proxy, unless
/// `$PACKCTL_CF_API_BASE` or `[pack] api_base` says otherwise. Modrinth needs
/// no key and talks to `$PACKCTL_MODRINTH_API_BASE` or the public API. Local
/// archives need no credentials, and a local pack without an `archive` reads
/// zips and `.mrpack` files dropped into `<server root>/packs`.
///
/// Modrinth packs (remote or local `.mrpack`) reuse identical files already
/// on the server instead of downloading them again.
///
/// Every provider keeps pack content away from the profile's reserved paths
/// ([`ReservedPaths::for_profile`]) while preparing; the planner skips them
/// again when it builds the plan.
pub fn from_profile(profile: &ServerProfile) -> Result<Box<dyn PackProvider>> {
    let reserved = ReservedPaths::for_profile(profile);
    match profile.pack.provider {
        ProviderKind::CurseForge => {
            let client = curseforge::client::CfClient::for_profile(profile)?;
            Ok(Box::new(
                curseforge::installer::CurseForgeProvider::new(client).with_reserved(reserved),
            ))
        }
        ProviderKind::Modrinth => Ok(Box::new(modrinth::provider::ModrinthProvider {
            client: modrinth::client::MrClient::from_env()?,
            fetcher: Arc::new(http::HttpFetcher::new()),
            reuse_root: Some(profile.server.root.clone()),
            optional_files: profile.pack.optional_files,
            download_concurrency: modrinth::provider::DEFAULT_DOWNLOAD_CONCURRENCY,
            reserved,
        })),
        ProviderKind::Local => {
            let archive = profile
                .pack
                .archive
                .clone()
                .unwrap_or_else(|| profile.server.root.join(DEFAULT_DROP_DIR));
            Ok(Box::new(
                local::LocalArchiveProvider::new(archive)
                    .with_reuse_root(profile.server.root.clone())
                    .with_optional_files(profile.pack.optional_files)
                    .with_reserved(reserved),
            ))
        }
    }
}

/// The upstream pack a profile follows; an absent id or slug is empty.
pub fn pack_ref(profile: &ServerProfile) -> PackRef {
    PackRef {
        project_id: profile.pack.project_id.clone().unwrap_or_default(),
        slug: profile.pack.slug.clone().unwrap_or_default(),
    }
}

/// Guesses the provider from a `--source` value.
///
/// A Modrinth or CurseForge URL names its provider, and a `.zip` or
/// `.mrpack` file name is a local archive. A bare id or slug is ambiguous
/// and yields `None`.
pub fn detect_provider(input: &str) -> Option<ProviderKind> {
    let lower = input.trim().to_lowercase();
    if lower.contains("modrinth.com") {
        Some(ProviderKind::Modrinth)
    } else if lower.contains("curseforge.com") {
        Some(ProviderKind::CurseForge)
    } else if lower.ends_with(".zip") || lower.ends_with(".mrpack") {
        Some(ProviderKind::Local)
    } else {
        None
    }
}

/// Resolves a version selector against a version list.
///
/// Providers must return their lists sorted newest first, because `Latest`
/// picks the first entry. `pack_label` names the pack in error messages, for
/// example `pack 'atm10' (project 925200)`.
///
/// - `Latest`: the first entry; an empty list is [`PackError::NotFound`].
/// - `Id`: the exact provider id.
/// - `Name`: the exact name, case-insensitively.
/// - `Query`: user input, tried in order as an exact id, an exact
///   case-insensitive name, an exact version number, and a unique token
///   match (see [`name_tokens`]).
///
/// More than one match is an ambiguity error ([`PackError::Config`]) that
/// lists the candidates; no match is [`PackError::NotFound`] with "did you
/// mean" suggestions.
pub(crate) fn resolve_version_from_list<'a>(
    versions: &'a [PackVersion],
    selector: &VersionSelector,
    pack_label: &str,
) -> Result<&'a PackVersion> {
    match selector {
        VersionSelector::Latest => versions
            .first()
            .ok_or_else(|| PackError::NotFound(format!("no versions available for {pack_label}"))),
        VersionSelector::Id(id) => {
            let id = id.trim();
            versions
                .iter()
                .find(|v| matches_id(v, id))
                .ok_or_else(|| not_found(versions, id, pack_label))
        }
        VersionSelector::Name(name) => {
            let name = name.trim();
            let matches = name_matches(versions, name);
            single_match(matches, versions, name, pack_label)
        }
        VersionSelector::Query(query) => {
            let query = query.trim();
            if let Some(version) = versions.iter().find(|v| matches_id(v, query)) {
                return Ok(version);
            }
            let by_name = name_matches(versions, query);
            if !by_name.is_empty() {
                return single_match(by_name, versions, query, pack_label);
            }
            let by_number: Vec<&PackVersion> = versions
                .iter()
                .filter(|v| v.number.as_deref() == Some(query))
                .collect();
            if !by_number.is_empty() {
                return single_match(by_number, versions, query, pack_label);
            }
            let wanted = name_tokens(query);
            let by_token: Vec<&PackVersion> = versions
                .iter()
                .filter(|v| contains_tokens(&name_tokens(&v.name), &wanted))
                .collect();
            single_match(by_token, versions, query, pack_label)
        }
    }
}

fn matches_id(version: &PackVersion, id: &str) -> bool {
    version.id == id
}

fn name_matches<'a>(versions: &'a [PackVersion], name: &str) -> Vec<&'a PackVersion> {
    let needle = name.to_lowercase();
    versions
        .iter()
        .filter(|v| v.name.to_lowercase() == needle)
        .collect()
}

/// Exactly one candidate wins; several are ambiguous; none is not found.
fn single_match<'a>(
    matches: Vec<&'a PackVersion>,
    versions: &'a [PackVersion],
    query: &str,
    pack_label: &str,
) -> Result<&'a PackVersion> {
    match matches.as_slice() {
        [single] => Ok(single),
        [] => Err(not_found(versions, query, pack_label)),
        several => Err(ambiguous(several, query, pack_label)),
    }
}

/// Most candidates listed in an ambiguity error.
const MAX_AMBIGUOUS_LISTED: usize = 8;

/// Most names offered as "did you mean" suggestions.
const MAX_SUGGESTIONS: usize = 5;

fn ambiguous(matches: &[&PackVersion], query: &str, pack_label: &str) -> PackError {
    let mut list = matches
        .iter()
        .take(MAX_AMBIGUOUS_LISTED)
        .map(|v| format!("{} (id {})", v.name, v.id))
        .collect::<Vec<_>>()
        .join(", ");
    if matches.len() > MAX_AMBIGUOUS_LISTED {
        list.push_str(", …");
    }
    PackError::Config(format!(
        "version '{query}' matches {} versions of {pack_label}: {list}; use the id from 'packctl versions'",
        matches.len()
    ))
}

fn not_found(versions: &[PackVersion], query: &str, pack_label: &str) -> PackError {
    let mut message = format!("no version matching '{query}' for {pack_label}");
    let suggestions = suggestions(versions, query);
    if !suggestions.is_empty() {
        message.push_str(&format!("; did you mean: {}?", suggestions.join(", ")));
    }
    PackError::NotFound(message)
}

/// Up to [`MAX_SUGGESTIONS`] names that contain any of the query's tokens,
/// or else the newest names.
fn suggestions(versions: &[PackVersion], query: &str) -> Vec<String> {
    let tokens = name_tokens(query);
    let related: Vec<String> = versions
        .iter()
        .filter(|v| {
            let name = v.name.to_lowercase();
            tokens.iter().any(|token| name.contains(token.as_str()))
        })
        .take(MAX_SUGGESTIONS)
        .map(|v| v.name.clone())
        .collect();
    if !related.is_empty() {
        return related;
    }
    versions
        .iter()
        .take(MAX_SUGGESTIONS)
        .map(|v| v.name.clone())
        .collect()
}

/// Splits a version name into lowercase tokens.
///
/// Separators are whitespace and `-_+()[]`; a leading `v` before a digit is
/// dropped, so `ATM8-v1.1.0` yields `["atm8", "1.1.0"]`.
pub(crate) fn name_tokens(name: &str) -> Vec<String> {
    name.split(|c: char| c.is_whitespace() || "-_+()[]".contains(c))
        .filter(|token| !token.is_empty())
        .map(|token| {
            let token = token.to_lowercase();
            match token.strip_prefix('v') {
                Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest.to_string(),
                _ => token,
            }
        })
        .collect()
}

/// True when `wanted` appears as a contiguous run inside `tokens`.
fn contains_tokens(tokens: &[String], wanted: &[String]) -> bool {
    !wanted.is_empty() && tokens.windows(wanted.len()).any(|window| window == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(id: &str, name: &str) -> PackVersion {
        PackVersion {
            id: id.to_string(),
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// Newest first, like every provider returns them.
    fn atm8() -> Vec<PackVersion> {
        vec![
            version("5003", "All the Mods 8-1.2.0"),
            version("5002", "All the Mods 8-1.1.0"),
            version("5001", "All the Mods 8-1.0.0"),
        ]
    }

    const LABEL: &str = "pack 'atm8' (project 1)";

    fn query(q: &str) -> VersionSelector {
        VersionSelector::Query(q.to_string())
    }

    #[test]
    fn latest_is_first_and_empty_list_is_not_found() {
        let versions = atm8();
        assert_eq!(
            resolve_version_from_list(&versions, &VersionSelector::Latest, LABEL)
                .unwrap()
                .id,
            "5003"
        );
        let err = resolve_version_from_list(&[], &VersionSelector::Latest, LABEL).unwrap_err();
        assert!(
            matches!(&err, PackError::NotFound(m) if m == "no versions available for pack 'atm8' (project 1)"),
            "{err:?}"
        );
    }

    #[test]
    fn query_exact_id() {
        let versions = atm8();
        let found = resolve_version_from_list(&versions, &query("5002"), LABEL).unwrap();
        assert_eq!(found.name, "All the Mods 8-1.1.0");
        let found =
            resolve_version_from_list(&versions, &VersionSelector::Id("5001".into()), LABEL)
                .unwrap();
        assert_eq!(found.name, "All the Mods 8-1.0.0");
    }

    #[test]
    fn query_exact_name_case_insensitive() {
        let versions = atm8();
        let found =
            resolve_version_from_list(&versions, &query("  all the mods 8-1.0.0 "), LABEL).unwrap();
        assert_eq!(found.id, "5001");
        let found = resolve_version_from_list(
            &versions,
            &VersionSelector::Name("ALL THE MODS 8-1.2.0".into()),
            LABEL,
        )
        .unwrap();
        assert_eq!(found.id, "5003");
    }

    #[test]
    fn query_token_matches_unique() {
        let versions = atm8();
        let found = resolve_version_from_list(&versions, &query("1.1.0"), LABEL).unwrap();
        assert_eq!(found.id, "5002");
        // A leading 'v' is ignored on both sides.
        let found = resolve_version_from_list(&versions, &query("v1.2.0"), LABEL).unwrap();
        assert_eq!(found.id, "5003");
        // Several tokens must appear together, in order.
        let found = resolve_version_from_list(&versions, &query("8 1.0.0"), LABEL).unwrap();
        assert_eq!(found.id, "5001");
    }

    #[test]
    fn query_ambiguous_lists_candidates() {
        let versions = atm8();
        let err = resolve_version_from_list(&versions, &query("8"), LABEL).unwrap_err();
        let PackError::Config(message) = &err else {
            panic!("expected a Config error, got {err:?}");
        };
        assert!(
            message.starts_with("version '8' matches 3 versions of pack 'atm8' (project 1): "),
            "{message}"
        );
        assert!(
            message.contains("All the Mods 8-1.2.0 (id 5003)"),
            "{message}"
        );
        assert!(
            message.contains("All the Mods 8-1.0.0 (id 5001)"),
            "{message}"
        );
        assert!(
            message.ends_with("; use the id from 'packctl versions'"),
            "{message}"
        );
    }

    #[test]
    fn query_not_found_suggests() {
        let versions = atm8();
        let err = resolve_version_from_list(&versions, &query("1.3"), LABEL).unwrap_err();
        let PackError::NotFound(message) = &err else {
            panic!("expected NotFound, got {err:?}");
        };
        assert!(
            message.starts_with("no version matching '1.3' for pack 'atm8' (project 1)"),
            "{message}"
        );
        // Nothing contains "1.3", so the newest names are offered.
        assert!(
            message.ends_with(
                "; did you mean: All the Mods 8-1.2.0, All the Mods 8-1.1.0, All the Mods 8-1.0.0?"
            ),
            "{message}"
        );

        let err = resolve_version_from_list(&versions, &query("1.1"), LABEL).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("; did you mean: All the Mods 8-1.1.0?"),
            "{err}"
        );

        let err = resolve_version_from_list(&[], &query("1.1"), LABEL).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no version matching '1.1' for pack 'atm8' (project 1)"
        );
    }

    #[test]
    fn name_duplicate_is_ambiguous() {
        let versions = vec![version("b2", "1.0.0"), version("a1", "1.0.0")];
        for selector in [query("1.0.0"), VersionSelector::Name("1.0.0".into())] {
            let err = resolve_version_from_list(&versions, &selector, LABEL).unwrap_err();
            assert!(
                matches!(&err, PackError::Config(m) if m.contains("matches 2 versions")
                    && m.contains("1.0.0 (id b2)")
                    && m.contains("1.0.0 (id a1)")),
                "{err:?}"
            );
        }
        // The id still picks one exactly.
        assert_eq!(
            resolve_version_from_list(&versions, &query("a1"), LABEL)
                .unwrap()
                .id,
            "a1"
        );
    }

    #[test]
    fn ambiguity_lists_at_most_eight_candidates() {
        let versions: Vec<PackVersion> = (0..10)
            .map(|i| version(&format!("id{i}"), &format!("Pack 8-{i}")))
            .collect();
        let err = resolve_version_from_list(&versions, &query("8"), LABEL).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("matches 10 versions"), "{message}");
        assert_eq!(message.matches("(id ").count(), 8, "{message}");
    }

    #[test]
    fn detect_provider_table() {
        let cases = [
            (
                "https://www.curseforge.com/minecraft/modpacks/all-the-mods-10",
                Some(ProviderKind::CurseForge),
            ),
            (
                "https://modrinth.com/modpack/fabulously-optimized",
                Some(ProviderKind::Modrinth),
            ),
            (
                "HTTPS://MODRINTH.COM/modpack/x",
                Some(ProviderKind::Modrinth),
            ),
            ("ServerPack-1.0.zip", Some(ProviderKind::Local)),
            ("packs/Pack 2.0.MRPACK", Some(ProviderKind::Local)),
            ("all-the-mods-10", None),
            ("925200", None),
            ("", None),
        ];
        for (input, expected) in cases {
            assert_eq!(detect_provider(input), expected, "input: {input}");
        }
    }

    #[test]
    fn id_selector_matches_only_id() {
        // A version whose name or number looks like another version's id
        // must not be picked by an id selector.
        let versions = vec![
            PackVersion {
                id: "AbC123".into(),
                name: "XyZ789".into(),
                number: Some("1.0.0".into()),
                ..Default::default()
            },
            PackVersion {
                id: "XyZ789".into(),
                name: "Second".into(),
                number: Some("2.0.0".into()),
                ..Default::default()
            },
        ];
        let id = |s: &str| VersionSelector::Id(s.to_string());
        assert_eq!(
            resolve_version_from_list(&versions, &id("XyZ789"), LABEL)
                .unwrap()
                .name,
            "Second"
        );
        for missing in ["1.0.0", "abc123", "Second"] {
            assert!(
                matches!(
                    resolve_version_from_list(&versions, &id(missing), LABEL),
                    Err(PackError::NotFound(_))
                ),
                "{missing}"
            );
        }
    }

    #[test]
    fn query_matches_version_number() {
        let versions = vec![
            PackVersion {
                id: "b".into(),
                name: "Fabulously Optimized".into(),
                number: Some("6.4.0".into()),
                ..Default::default()
            },
            PackVersion {
                id: "a".into(),
                name: "Fabulously Optimized".into(),
                number: Some("6.3.0".into()),
                ..Default::default()
            },
        ];
        // Names are duplicated (common on Modrinth), so the number decides.
        assert_eq!(
            resolve_version_from_list(&versions, &query("6.3.0"), LABEL)
                .unwrap()
                .id,
            "a"
        );
        // The name alone is still ambiguous.
        assert!(matches!(
            resolve_version_from_list(&versions, &query("fabulously optimized"), LABEL),
            Err(PackError::Config(_))
        ));
    }

    #[test]
    fn requirements_describe_omits_missing_parts() {
        let full = PackRequirements {
            minecraft: Some("1.21.1".into()),
            loader: Some(LoaderRequirement {
                kind: LoaderKind::NeoForge,
                version: "21.1.77".into(),
            }),
        };
        assert_eq!(full.describe(), "Minecraft 1.21.1 · NeoForge 21.1.77");
        let loader_only = PackRequirements {
            minecraft: None,
            ..full.clone()
        };
        assert_eq!(loader_only.describe(), "NeoForge 21.1.77");
        assert_eq!(PackRequirements::default().describe(), "");
        assert_eq!(LoaderKind::Fabric.to_string(), "Fabric");
        assert_eq!(LoaderKind::Quilt.to_string(), "Quilt");
        assert_eq!(LoaderKind::Forge.to_string(), "Forge");
    }

    #[test]
    fn name_tokens_split_and_strip_leading_v() {
        assert_eq!(
            name_tokens("ATM8-v1.1.0 (Server)"),
            vec!["atm8", "1.1.0", "server"]
        );
        assert_eq!(name_tokens("vanilla+"), vec!["vanilla"]);
        assert!(name_tokens(" -_ ").is_empty());
    }
}
