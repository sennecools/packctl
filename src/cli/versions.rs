//! `packctl versions` — list available upstream versions for a server.

use crate::error::{PackError, Result};
use crate::providers::PackVersion;

/// Lists the upstream versions a server's pack can be updated to.
pub async fn run(server: Option<&str>, json: bool) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let provider = crate::providers::from_profile(&profile)?;

    let versions = provider
        .list_versions(&crate::providers::pack_ref(&profile))
        .await?;
    if versions.is_empty() {
        return Err(PackError::Provider(format!(
            "no versions available for server '{}'",
            profile.name
        )));
    }

    if json {
        let output = versions_json(&versions)?;
        outln!("{output}");
    } else {
        out!("{}", versions_list(&profile.name, &versions));
    }
    Ok(())
}

/// Renders the human-readable numbered version list.
///
/// Each line is `N. name (id ID)`; a version number that differs from the
/// name is shown after it, and known Minecraft versions and loaders follow
/// in brackets.
fn versions_list(server_name: &str, versions: &[PackVersion]) -> String {
    let mut lines = vec![format!("Available versions for {server_name}")];
    lines.extend(
        versions
            .iter()
            .enumerate()
            .map(|(index, version)| format!("  {}. {}", index + 1, version_line(version))),
    );
    lines.join("\n") + "\n"
}

fn version_line(version: &PackVersion) -> String {
    let mut line = version.name.clone();
    if let Some(number) = version
        .number
        .as_deref()
        .filter(|number| *number != version.name)
    {
        line.push_str(&format!(" · {number}"));
    }
    line.push_str(&format!(" (id {})", version.id));
    let tags: Vec<String> = [&version.game_versions, &version.loaders]
        .into_iter()
        .filter(|list| !list.is_empty())
        .map(|list| list.join(","))
        .collect();
    if !tags.is_empty() {
        line.push_str(&format!(" [{}]", tags.join(" ")));
    }
    line
}

/// Renders the versions as a JSON array of
/// `{id, name, number, released, game_versions, loaders}` objects.
fn versions_json(versions: &[PackVersion]) -> Result<String> {
    let items: Vec<serde_json::Value> = versions
        .iter()
        .map(|version| {
            serde_json::json!({
                "id": &version.id,
                "name": &version.name,
                "number": &version.number,
                "released": &version.released,
                "game_versions": &version.game_versions,
                "loaders": &version.loaders,
            })
        })
        .collect();
    serde_json::to_string_pretty(&items)
        .map_err(|err| PackError::Parse(format!("failed to serialize versions: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(id: &str, name: &str, released: Option<&str>) -> PackVersion {
        PackVersion {
            id: id.to_string(),
            name: name.to_string(),
            released: released.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn versions_list_is_numbered_with_ids() {
        let versions = vec![version("123", "4.11.1", None), version("124", "4.12", None)];

        let list = versions_list("ATM10", &versions);

        assert!(list.contains("Available versions for ATM10"));
        assert!(list.contains("  1. 4.11.1 (id 123)\n"), "{list}");
        assert!(list.contains("  2. 4.12 (id 124)\n"), "{list}");
    }

    #[test]
    fn list_shows_number_and_loaders() {
        let versions = vec![
            PackVersion {
                number: Some("6.4.0".to_string()),
                game_versions: vec!["1.21.1".to_string(), "1.21".to_string()],
                loaders: vec!["fabric".to_string()],
                ..version("tXKSV9ql", "Fabulously Optimized", None)
            },
            // A number equal to the name is not repeated.
            PackVersion {
                number: Some("2.0".to_string()),
                loaders: vec!["neoforge".to_string()],
                ..version("b", "2.0", None)
            },
        ];

        let list = versions_list("FO", &versions);

        assert!(
            list.contains("  1. Fabulously Optimized · 6.4.0 (id tXKSV9ql) [1.21.1,1.21 fabric]\n"),
            "{list}"
        );
        assert!(list.contains("  2. 2.0 (id b) [neoforge]\n"), "{list}");
    }

    #[test]
    fn versions_json_contains_ids_names_and_releases() {
        let versions = vec![
            version("123", "4.11.1", Some("2026-08-01T00:00:00Z")),
            version("124", "4.12", None),
        ];

        let json = versions_json(&versions).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed[0]["id"], "123");
        assert_eq!(parsed[0]["name"], "4.11.1");
        assert_eq!(parsed[0]["released"], "2026-08-01T00:00:00Z");
        assert_eq!(parsed[1]["id"], "124");
        assert_eq!(parsed[1]["name"], "4.12");
        assert!(parsed[1]["released"].is_null());
    }

    #[test]
    fn json_includes_new_fields() {
        let versions = vec![
            PackVersion {
                number: Some("6.4.0".to_string()),
                game_versions: vec!["1.21.1".to_string()],
                loaders: vec!["fabric".to_string(), "quilt".to_string()],
                ..version("a", "FO", None)
            },
            version("b", "plain", None),
        ];

        let parsed: serde_json::Value =
            serde_json::from_str(&versions_json(&versions).unwrap()).unwrap();

        assert_eq!(parsed[0]["number"], "6.4.0");
        assert_eq!(parsed[0]["game_versions"], serde_json::json!(["1.21.1"]));
        assert_eq!(parsed[0]["loaders"], serde_json::json!(["fabric", "quilt"]));
        assert!(parsed[1]["number"].is_null());
        assert_eq!(parsed[1]["game_versions"], serde_json::json!([]));
        assert_eq!(parsed[1]["loaders"], serde_json::json!([]));
    }
}
