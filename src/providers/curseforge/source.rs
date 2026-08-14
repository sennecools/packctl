//! CurseForge project source parsing.
//!
//! A "source" is what the user types when setting up a server: a project URL,
//! a numeric project id, or a slug. Parsing it into a [`ProjectSource`] lets
//! the create flow resolve to a project id without the user knowing it.

use crate::error::{PackError, Result};

/// A project source as typed by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSource {
    /// A numeric CurseForge project id.
    Id(u32),
    /// A project slug, resolved through the API.
    Slug(String),
}

/// Path segments that introduce a project slug in a CurseForge URL.
const PROJECT_KEYWORDS: &[&str] = &["modpacks", "mc-mods"];

/// Parses user input into a [`ProjectSource`].
///
/// Accepts:
/// - a bare numeric project id (`925200`)
/// - a CurseForge project URL on any `curseforge.com` host, e.g.
///   `https://www.curseforge.com/minecraft/modpacks/all-the-mods-10`
/// - a bare slug (`all-the-mods-10`)
pub fn parse_project_source(input: &str) -> Result<ProjectSource> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(PackError::Config(
            "pack source must be a CurseForge project URL, project ID, or slug".to_string(),
        ));
    }

    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        let id = trimmed
            .parse()
            .map_err(|_| PackError::Config(format!("invalid CurseForge project ID '{trimmed}'")))?;
        return Ok(ProjectSource::Id(id));
    }

    if trimmed.to_lowercase().contains("curseforge.com") {
        return parse_curseforge_url(trimmed);
    }

    Ok(ProjectSource::Slug(slugify(trimmed)))
}

/// Extracts a project slug or id from a CurseForge URL.
fn parse_curseforge_url(input: &str) -> Result<ProjectSource> {
    let lower = input.to_lowercase();
    let rest = lower.split("://").last().unwrap_or(&lower);
    let path_and_query = rest.split(['?', '#']).next().unwrap_or(rest);
    let segments: Vec<&str> = path_and_query
        .split('/')
        .map(|segment| segment.trim())
        .filter(|segment| !segment.is_empty())
        .collect();

    for (index, segment) in segments.iter().enumerate() {
        if PROJECT_KEYWORDS.contains(segment) {
            let Some(next) = segments.get(index + 1) else {
                break;
            };
            if let Ok(id) = next.parse::<u32>() {
                return Ok(ProjectSource::Id(id));
            }
            return Ok(ProjectSource::Slug(slugify(next)));
        }
    }

    Err(PackError::Config(format!(
        "could not parse a project slug or ID from '{input}'"
    )))
}

/// Normalizes a slug fragment: trims and lowercases.
fn slugify(segment: &str) -> String {
    segment.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_numeric_input_is_project_id() {
        assert_eq!(
            parse_project_source("925200").unwrap(),
            ProjectSource::Id(925200)
        );
        assert_eq!(parse_project_source("  1  ").unwrap(), ProjectSource::Id(1));
    }

    #[test]
    fn modpack_url_extracts_slug() {
        for input in [
            "https://www.curseforge.com/minecraft/modpacks/all-the-mods-10",
            "https://www.curseforge.com/minecraft/modpacks/all-the-mods-10/files",
            "https://www.curseforge.com/minecraft/modpacks/all-the-mods-10/files/5560430",
            "https://www.curseforge.com/minecraft/modpacks/all-the-mods-10?gameVersionTypeId=5",
            "www.curseforge.com/minecraft/modpacks/all-the-mods-10",
            "https://legacy.curseforge.com/minecraft/modpacks/all-the-mods-10",
        ] {
            assert_eq!(
                parse_project_source(input).unwrap(),
                ProjectSource::Slug("all-the-mods-10".to_string()),
                "input: {input}"
            );
        }
    }

    #[test]
    fn numeric_modpack_url_is_project_id() {
        let parsed =
            parse_project_source("https://www.curseforge.com/minecraft/modpacks/925200").unwrap();
        assert_eq!(parsed, ProjectSource::Id(925200));
    }

    #[test]
    fn mc_mod_url_extracts_slug() {
        let parsed =
            parse_project_source("https://www.curseforge.com/minecraft/mc-mods/jei").unwrap();
        assert_eq!(parsed, ProjectSource::Slug("jei".to_string()));
    }

    #[test]
    fn bare_slug_is_passthrough() {
        assert_eq!(
            parse_project_source("all-the-mods-10").unwrap(),
            ProjectSource::Slug("all-the-mods-10".to_string())
        );
        assert_eq!(
            parse_project_source("  ALL-The-Mods-10 ").unwrap(),
            ProjectSource::Slug("all-the-mods-10".to_string())
        );
    }

    #[test]
    fn empty_input_errors() {
        assert!(matches!(
            parse_project_source("   "),
            Err(PackError::Config(_))
        ));
    }

    #[test]
    fn malformed_curseforge_url_errors() {
        assert!(matches!(
            parse_project_source("https://www.curseforge.com/minecraft"),
            Err(PackError::Config(_))
        ));
    }
}
