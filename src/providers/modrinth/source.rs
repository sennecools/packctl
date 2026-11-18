//! Parsing user-supplied Modrinth pack sources: a modrinth.com URL, a
//! project id, or a slug.

use reqwest::Url;

use crate::error::{PackError, Result};
use crate::providers::sanitize;

use super::client::{ENV_API_BASE, MrClient, STAGING_API_BASE, env_api_base};
use super::models::MrProject;

/// A parsed Modrinth source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModrinthSource {
    /// The project id or slug, as given (ids are case-sensitive).
    pub project: String,
    /// A version the URL pinned (`.../version/{v}`), if any.
    pub version: Option<String>,
    /// True for a staging.modrinth.com URL.
    pub staging: bool,
}

/// Website hosts whose URLs name Modrinth projects.
const HOSTS: &[&str] = &["modrinth.com", "www.modrinth.com", "staging.modrinth.com"];

/// Project types that appear as the first URL path segment.
const PROJECT_TYPES: &[&str] = &[
    "modpack",
    "project",
    "mod",
    "plugin",
    "datapack",
    "resourcepack",
    "shader",
];

/// Project page tabs that may follow the project in a URL; they name the
/// project itself.
const PROJECT_TABS: &[&str] = &[
    "description",
    "versions",
    "gallery",
    "changelog",
    "settings",
    "moderation",
];

/// Characters Modrinth allows in ids and slugs, besides ASCII alphanumerics.
const SLUG_PUNCTUATION: &str = "!@$()`.+,\"'_-";

/// True for a bare id or slug: 1 to 64 characters from Modrinth's slug
/// alphabet, and not `.` or `..`.
fn is_bare_id(input: &str) -> bool {
    (1..=64).contains(&input.chars().count())
        && input
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || SLUG_PUNCTUATION.contains(c))
        && !matches!(input, "." | "..")
}

/// Parses a Modrinth project URL, id or slug.
///
/// URLs on modrinth.com, www.modrinth.com and staging.modrinth.com with the
/// path `/{type}/{id or slug}[/version/{version}]` or
/// `/{type}/{id or slug}/{tab}` (`versions`, `gallery`, `changelog`, ...) are
/// accepted (the scheme may be left out); queries and fragments are ignored.
/// A bare id or slug is kept exactly, since base62 ids are case-sensitive.
pub fn parse_modrinth_source(input: &str) -> Result<ModrinthSource> {
    let trimmed = input.trim();
    let not_modrinth = || {
        PackError::Config(format!(
            "'{}' is not a Modrinth project URL, id, or slug",
            sanitize(trimmed)
        ))
    };
    if is_bare_id(trimmed) {
        return Ok(ModrinthSource {
            project: trimmed.to_string(),
            version: None,
            staging: false,
        });
    }

    let lower = trimmed.to_ascii_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        trimmed.to_string()
    } else if HOSTS
        .iter()
        .any(|host| lower.starts_with(&format!("{host}/")))
    {
        format!("https://{trimmed}")
    } else {
        return Err(not_modrinth());
    };
    let url = Url::parse(&with_scheme).map_err(|_| not_modrinth())?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if !HOSTS.contains(&host.as_str()) {
        return Err(not_modrinth());
    }
    let segments: Vec<String> = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .map(percent_decode)
                .collect()
        })
        .unwrap_or_default();
    let (kind, project, version) = match segments.as_slice() {
        [kind, project] => (kind, project, None),
        [kind, project, tab] if PROJECT_TABS.contains(&tab.to_ascii_lowercase().as_str()) => {
            (kind, project, None)
        }
        [kind, project, version_word, version] if version_word == "version" => {
            (kind, project, Some(version.clone()))
        }
        _ => return Err(not_modrinth()),
    };
    if !PROJECT_TYPES.contains(&kind.to_ascii_lowercase().as_str()) || !is_bare_id(project) {
        return Err(not_modrinth());
    }
    Ok(ModrinthSource {
        project: project.clone(),
        version,
        staging: host == "staging.modrinth.com",
    })
}

/// Decodes `%XX` escapes in a URL path segment; invalid escapes stay as is.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = segment.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Rejects projects packctl cannot install on a server: anything that is
/// not a modpack, and client-only modpacks.
pub fn check_server_modpack(project: &MrProject) -> Result<()> {
    let slug = sanitize(&project.slug);
    if project.project_type != "modpack" {
        return Err(PackError::Config(format!(
            "'{slug}' is a Modrinth {}, not a modpack",
            sanitize(&project.project_type)
        )));
    }
    if project.server_side.as_deref() == Some("unsupported") {
        return Err(PackError::Config(format!(
            "'{slug}' is a client-only Modrinth modpack (server_side = unsupported)"
        )));
    }
    Ok(())
}

/// Resolves user input to a server-capable Modrinth modpack, plus the
/// version a URL pinned.
pub async fn resolve(client: &MrClient, input: &str) -> Result<(MrProject, Option<String>)> {
    let source = parse_modrinth_source(input)?;
    if source.staging && env_api_base().is_none() {
        errln!(
            "hint: this is a staging.modrinth.com URL; set {ENV_API_BASE}={STAGING_API_BASE} to \
             use the staging API"
        );
    }
    let project = client.get_project(&source.project).await?;
    check_server_modpack(&project)?;
    Ok((project, source.version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::modrinth::models::fixtures;

    fn parsed(project: &str, version: Option<&str>, staging: bool) -> ModrinthSource {
        ModrinthSource {
            project: project.to_string(),
            version: version.map(str::to_string),
            staging,
        }
    }

    #[test]
    fn parse_modrinth_urls() {
        let cases = [
            (
                "https://modrinth.com/modpack/cobblemon-modpack",
                parsed("cobblemon-modpack", None, false),
            ),
            (
                "https://www.modrinth.com/project/5FFgwNNP",
                parsed("5FFgwNNP", None, false),
            ),
            (
                "https://modrinth.com/modpack/adrenaserver/version/1.6.0+1.21.1.fabric",
                parsed("adrenaserver", Some("1.6.0+1.21.1.fabric"), false),
            ),
            (
                "https://staging.modrinth.com/modpack/test-pack",
                parsed("test-pack", None, true),
            ),
            (
                "https://modrinth.com/modpack/cobblemon-modpack?tab=versions#top",
                parsed("cobblemon-modpack", None, false),
            ),
            (
                "modrinth.com/modpack/cobblemon-modpack/",
                parsed("cobblemon-modpack", None, false),
            ),
            (
                "HTTPS://MODRINTH.COM/Modpack/H9OFWiay",
                parsed("H9OFWiay", None, false),
            ),
            (
                "https://modrinth.com/modpack/cobblemon-modpack/versions?g=1.21.1",
                parsed("cobblemon-modpack", None, false),
            ),
            (
                "https://modrinth.com/modpack/cobblemon-modpack/gallery",
                parsed("cobblemon-modpack", None, false),
            ),
            (
                "modrinth.com/modpack/cobblemon-modpack/Changelog/",
                parsed("cobblemon-modpack", None, false),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_modrinth_source(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn bare_id_keeps_case() {
        assert_eq!(
            parse_modrinth_source("  5FFgwNNP ").unwrap(),
            parsed("5FFgwNNP", None, false)
        );
        assert_eq!(
            parse_modrinth_source("Fabulously_Optimized.2").unwrap(),
            parsed("Fabulously_Optimized.2", None, false)
        );
    }

    #[test]
    fn rejects_garbage() {
        for input in [
            "",
            "..",
            "has space",
            "https://example.com/modpack/x",
            "https://modrinth.com.evil.example/modpack/x",
            "https://modrinth.com/modpack",
            "https://modrinth.com/user/someone",
            "https://modrinth.com/modpack/x/changelog/extra",
            "https://modrinth.com/modpack/x/wiki",
            "https://modrinth.com/modpack/x/versions/1.0",
            "https://modrinth.com/modpack/%2E%2E",
            &"a".repeat(65),
        ] {
            let err = parse_modrinth_source(input).unwrap_err();
            assert!(
                matches!(&err, PackError::Config(m) if m.ends_with("is not a Modrinth project URL, id, or slug")),
                "{input}: {err:?}"
            );
        }
    }

    #[test]
    fn modpack_and_server_side_checks() {
        let project: MrProject = serde_json::from_str(fixtures::PROJECT).unwrap();
        check_server_modpack(&project).unwrap();

        let client_only = MrProject {
            slug: "fabulously-optimized".to_string(),
            server_side: Some("unsupported".to_string()),
            ..project.clone()
        };
        assert_eq!(
            check_server_modpack(&client_only).unwrap_err().to_string(),
            "'fabulously-optimized' is a client-only Modrinth modpack (server_side = unsupported)"
        );

        let a_mod = MrProject {
            slug: "sodium".to_string(),
            project_type: "mod".to_string(),
            ..project
        };
        assert_eq!(
            check_server_modpack(&a_mod).unwrap_err().to_string(),
            "'sodium' is a Modrinth mod, not a modpack"
        );
    }
}
