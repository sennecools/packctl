//! `packctl versions` — list available upstream versions for a server.

use crate::core::updater::Updater;
use crate::error::{PackError, Result};
use crate::providers::PackVersion;

/// Lists the upstream versions a server's pack can be updated to.
pub async fn run(server: Option<&str>, json: bool) -> Result<()> {
    let profile = crate::config::profile::resolve_profile(server)?;
    let updater = Updater::from_profile(&profile)?;

    let versions = updater.provider.list_versions(&updater.pack_ref()).await?;
    if versions.is_empty() {
        return Err(PackError::Provider(format!(
            "no versions available for server '{}'",
            profile.name
        )));
    }

    if json {
        let output = versions_json(&versions)?;
        println!("{output}");
    } else {
        print!("{}", versions_list(&profile.name, &versions));
    }
    Ok(())
}

/// Renders the human-readable numbered version list.
fn versions_list(server_name: &str, versions: &[PackVersion]) -> String {
    let mut lines = vec![format!("Available versions for {server_name}")];
    lines.extend(
        versions.iter().enumerate().map(|(index, version)| {
            format!("  {}. {} (id {})", index + 1, version.name, version.id)
        }),
    );
    lines.join("\n") + "\n"
}

/// Renders the versions as a JSON array of `{id, name, released}` objects.
fn versions_json(versions: &[PackVersion]) -> Result<String> {
    let items: Vec<serde_json::Value> = versions
        .iter()
        .map(|version| {
            serde_json::json!({
                "id": &version.id,
                "name": &version.name,
                "released": &version.released,
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
            file_id: None,
            released: released.map(str::to_string),
        }
    }

    #[test]
    fn versions_list_is_numbered_with_ids() {
        let versions = vec![version("123", "4.11.1", None), version("124", "4.12", None)];

        let list = versions_list("ATM10", &versions);

        assert!(list.contains("Available versions for ATM10"));
        assert!(list.contains("  1. 4.11.1 (id 123)"));
        assert!(list.contains("  2. 4.12 (id 124)"));
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
}
