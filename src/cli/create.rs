//! `packctl create` — interactively create a server profile.
//!
//! The command resolves a CurseForge project from a URL, project ID, or slug,
//! asks for the server root, overlay, and controller details, and writes a
//! profile file. Everything can be supplied via flags for non-interactive use.

use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::config::profile::{
    CommandConfig, ControllerKind, ProfileDraft, ProviderKind, write_profile,
};
use crate::error::{PackError, Result};
use crate::providers::curseforge::client::CfClient;
use crate::providers::curseforge::source::{ProjectSource, parse_project_source};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Arguments for `packctl create`.
pub struct CreateArgs {
    pub name: Option<String>,
    pub non_interactive: bool,
    pub force: bool,
    pub source: Option<String>,
    pub root: Option<PathBuf>,
    pub overlay: Option<PathBuf>,
    pub controller: Option<String>,
    pub instance: Option<String>,
    pub status: Option<String>,
    pub stop: Option<String>,
    pub start: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// A CurseForge project resolved from a user-supplied source.
struct ResolvedPack {
    name: String,
    slug: String,
    project_id: u32,
}

/// Result of choosing a server controller.
struct ControllerSelection {
    kind: ControllerKind,
    instance: Option<String>,
    command: Option<CommandConfig>,
}

/// Command-controller options collected from flags.
struct CommandOptions {
    status: Option<String>,
    stop: Option<String>,
    start: Option<String>,
    timeout_ms: Option<u64>,
}

impl From<&CreateArgs> for CommandOptions {
    fn from(args: &CreateArgs) -> Self {
        Self {
            status: args.status.clone(),
            stop: args.stop.clone(),
            start: args.start.clone(),
            timeout_ms: args.timeout_ms,
        }
    }
}

pub async fn run(args: CreateArgs) -> Result<()> {
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();

    let name = resolve_name(args.name.as_deref(), interactive)?;
    let client = CfClient::from_env()?;
    let pack = resolve_pack(&client, args.source.clone(), interactive).await?;

    if interactive {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!(
                "Found '{}' (project {})",
                pack.name, pack.project_id
            ))
            .default(true)
            .interact()
            .map_err(|err| dialoguer_error("confirm pack selection", err))?;
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    let cwd =
        env::current_dir().map_err(|err| PackError::io("determine current directory", err))?;
    let root = resolve_root(args.root.clone(), &cwd, interactive)?;
    let overlay = resolve_overlay(args.overlay.clone(), &root, interactive)?;
    let controller = select_controller(&args, &name, interactive).await?;

    let draft = ProfileDraft {
        name,
        server_root: root.clone(),
        provider: ProviderKind::CurseForge,
        project_id: pack.project_id,
        slug: (!pack.slug.is_empty()).then(|| pack.slug.clone()),
        overlay_path: overlay.clone(),
        controller: controller.kind,
        instance: controller.instance,
        command: controller.command,
    };

    let path = write_profile(&draft, args.force)?;
    println!();
    println!("Created profile '{}'", draft.name);
    println!("  file:       {}", path.display());
    println!("  pack:       {} (project {})", pack.name, pack.project_id);
    println!("  server:     {}", root.display());
    println!("  overlay:    {}", overlay.display());
    println!("  controller: {}", controller_description(&draft));
    println!();
    println!("Next: packctl status {}", draft.name);
    Ok(())
}

/// Resolves the profile name, prompting when interactive and omitted.
fn resolve_name(name: Option<&str>, interactive: bool) -> Result<String> {
    match name {
        Some(name) if !name.trim().is_empty() => Ok(name.to_string()),
        Some(_) => Err(PackError::Config(
            "profile name must not be empty".to_string(),
        )),
        None if interactive => dialoguer::Input::new()
            .with_prompt("Server profile name")
            .default(default_profile_name())
            .interact_text()
            .map_err(|err| dialoguer_error("read profile name", err)),
        None => Ok(default_profile_name()),
    }
}

/// The current directory name, used as the default profile name.
fn default_profile_name() -> String {
    env::current_dir()
        .ok()
        .and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "server".to_string())
}

/// Resolves the pack source to a concrete project, prompting when interactive.
async fn resolve_pack(
    client: &CfClient,
    source: Option<String>,
    interactive: bool,
) -> Result<ResolvedPack> {
    let input = match source {
        Some(source) => source,
        None if interactive => dialoguer::Input::new()
            .with_prompt("CurseForge modpack URL or project ID")
            .interact_text()
            .map_err(|err| dialoguer_error("read pack source", err))?,
        None => {
            return Err(PackError::Config(
                "pack source is required in non-interactive mode; pass --source <url|id>"
                    .to_string(),
            ));
        }
    };

    match parse_project_source(&input)? {
        ProjectSource::Id(id) => {
            // The id is authoritative; enrichment is best-effort so numeric
            // ids can be used without an API key.
            match client.get_mod(id).await {
                Ok(mod_info) => Ok(ResolvedPack {
                    name: mod_info.name,
                    slug: mod_info.slug,
                    project_id: id,
                }),
                Err(err) => {
                    eprintln!(
                        "note: could not look up project {id} ({err}); creating the profile anyway"
                    );
                    Ok(ResolvedPack {
                        name: format!("project {id}"),
                        slug: String::new(),
                        project_id: id,
                    })
                }
            }
        }
        ProjectSource::Slug(slug) => {
            let mod_info = client.search_by_slug(&slug).await.map_err(|err| {
                PackError::Provider(format!(
                    "could not resolve '{slug}' to a CurseForge project id: {err}\n\
                     Set the CF_API_KEY environment variable and retry, or pass the \
                     numeric project ID directly with --source <id>"
                ))
            })?;
            Ok(ResolvedPack {
                name: mod_info.name,
                slug: mod_info.slug,
                project_id: mod_info.id,
            })
        }
    }
}

/// Resolves the server root, defaulting to the current directory.
fn resolve_root(root: Option<PathBuf>, cwd: &Path, interactive: bool) -> Result<PathBuf> {
    let resolved = match root {
        Some(root) => absolute_path(&root, cwd),
        None if interactive => {
            let input: String = dialoguer::Input::new()
                .with_prompt("Server root")
                .default(cwd.display().to_string())
                .interact_text()
                .map_err(|err| dialoguer_error("read server root", err))?;
            absolute_path(Path::new(&input), cwd)
        }
        None => cwd.to_path_buf(),
    };
    Ok(resolved)
}

/// Resolves the overlay directory, defaulting to `<root>/overlay`.
fn resolve_overlay(overlay: Option<PathBuf>, root: &Path, interactive: bool) -> Result<PathBuf> {
    let default = root.join("overlay");
    let resolved = match overlay {
        Some(overlay) => {
            let cwd = env::current_dir()
                .map_err(|err| PackError::io("determine current directory", err))?;
            absolute_path(&overlay, &cwd)
        }
        None if interactive => {
            let input: String = dialoguer::Input::new()
                .with_prompt("Overlay directory")
                .default(default.display().to_string())
                .interact_text()
                .map_err(|err| dialoguer_error("read overlay directory", err))?;
            absolute_path(Path::new(&input), root.parent().unwrap_or(root))
        }
        None => default,
    };
    Ok(resolved)
}

/// Makes `path` absolute against `cwd` when it is relative.
fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Chooses the controller, prompting when interactive.
async fn select_controller(
    args: &CreateArgs,
    name: &str,
    interactive: bool,
) -> Result<ControllerSelection> {
    let kind = match &args.controller {
        Some(value) => parse_controller_kind(value)?,
        None if interactive => {
            let items = ["amp", "command"];
            let chosen = dialoguer::Select::new()
                .with_prompt("Server controller")
                .items(&items)
                .default(0)
                .interact()
                .map_err(|err| dialoguer_error("select controller", err))?;
            match items[chosen] {
                "amp" => ControllerKind::Amp,
                _ => ControllerKind::Command,
            }
        }
        None => {
            return Err(PackError::Config(
                "controller is required in non-interactive mode; pass --controller amp|command"
                    .to_string(),
            ));
        }
    };

    match kind {
        ControllerKind::Amp => {
            let instance = match &args.instance {
                Some(instance) => instance.clone(),
                None if interactive => dialoguer::Input::new()
                    .with_prompt("AMP instance name")
                    .default(name.to_string())
                    .interact_text()
                    .map_err(|err| dialoguer_error("read AMP instance", err))?,
                None => {
                    return Err(PackError::Config(
                        "AMP controller requires --instance in non-interactive mode".to_string(),
                    ));
                }
            };
            Ok(ControllerSelection {
                kind,
                instance: Some(instance),
                command: None,
            })
        }
        ControllerKind::Command => {
            let options = CommandOptions::from(args);
            let status = get_command(
                options.status,
                "--status",
                "Status command",
                "pgrep -f server.jar",
                interactive,
            )?;
            let stop = get_command(
                options.stop,
                "--stop",
                "Stop command",
                &format!("screen -S {name} -X stuff \"stop\\n\""),
                interactive,
            )?;
            let start = get_command(
                options.start,
                "--start",
                "Start command",
                &format!("screen -S {name} -X stuff \"start\\n\""),
                interactive,
            )?;
            let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
            Ok(ControllerSelection {
                kind,
                instance: None,
                command: Some(CommandConfig {
                    status,
                    stop,
                    start,
                    timeout_ms: Some(timeout_ms),
                }),
            })
        }
    }
}

/// Resolves a command-controller argv from a flag or an interactive prompt.
fn get_command(
    flag: Option<String>,
    flag_name: &str,
    prompt: &str,
    default: &str,
    interactive: bool,
) -> Result<Vec<String>> {
    let line = match flag {
        Some(value) => value,
        None if interactive => dialoguer::Input::new()
            .with_prompt(prompt)
            .default(default.to_string())
            .interact_text()
            .map_err(|err| dialoguer_error("read command", err))?,
        None => {
            return Err(PackError::Config(format!(
                "command controller requires {flag_name} in non-interactive mode"
            )));
        }
    };
    let argv = argv_split(&line);
    if argv.is_empty() {
        return Err(PackError::Config(format!("{prompt} must not be empty")));
    }
    Ok(argv)
}

/// Splits a shell-like command line into argv tokens.
///
/// Whitespace separates tokens; double quotes group tokens and support the
/// `\n`, `\t`, `\\`, and `\"` escapes. Commands are never run through a shell,
/// so this splitter is intentionally small and predictable.
fn argv_split(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;

    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '\\' => match chars.peek() {
                Some('n') => {
                    chars.next();
                    current.push('\n');
                }
                Some('t') => {
                    chars.next();
                    current.push('\t');
                }
                Some('"') => {
                    chars.next();
                    current.push('"');
                }
                Some('\\') => {
                    chars.next();
                    current.push('\\');
                }
                _ => current.push('\\'),
            },
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_controller_kind(value: &str) -> Result<ControllerKind> {
    match value.to_lowercase().as_str() {
        "amp" => Ok(ControllerKind::Amp),
        "command" => Ok(ControllerKind::Command),
        other => Err(PackError::Config(format!(
            "unknown controller '{other}'; expected 'amp' or 'command'"
        ))),
    }
}

fn controller_description(draft: &ProfileDraft) -> String {
    match &draft.command {
        Some(_) => "command".to_string(),
        None => format!("amp (instance {})", draft.instance.as_deref().unwrap_or("")),
    }
}

/// Wraps a [`dialoguer::Error`] with the operation that failed.
fn dialoguer_error(what: &str, err: dialoguer::Error) -> PackError {
    match err {
        dialoguer::Error::IO(source) => PackError::io(what, source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_split_handles_quotes_and_escapes() {
        assert_eq!(
            argv_split("screen -S atm10 -X stuff \"stop\\n\""),
            vec!["screen", "-S", "atm10", "-X", "stuff", "stop\n",]
        );
        assert_eq!(
            argv_split("pgrep -f 'server.jar'"),
            vec!["pgrep", "-f", "'server.jar'"]
        );
        assert_eq!(argv_split("echo \"a b\"  c"), vec!["echo", "a b", "c"]);
        assert_eq!(argv_split("  "), Vec::<String>::new());
        assert_eq!(argv_split(""), Vec::<String>::new());
    }

    #[test]
    fn parse_controller_kind_accepts_amp_and_command() {
        assert_eq!(parse_controller_kind("amp").unwrap(), ControllerKind::Amp);
        assert_eq!(
            parse_controller_kind("command").unwrap(),
            ControllerKind::Command
        );
        assert_eq!(
            parse_controller_kind("COMMAND").unwrap(),
            ControllerKind::Command
        );
        assert!(matches!(
            parse_controller_kind("docker"),
            Err(PackError::Config(_))
        ));
    }

    #[test]
    fn absolute_path_joins_against_cwd() {
        assert_eq!(
            absolute_path(Path::new("server"), Path::new("/srv")),
            PathBuf::from("/srv/server")
        );
        assert_eq!(
            absolute_path(Path::new("/abs/path"), Path::new("/srv")),
            PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn controller_description_reports_kind() {
        let amp = ProfileDraft {
            name: "atm10".to_string(),
            server_root: PathBuf::from("/srv"),
            provider: ProviderKind::CurseForge,
            project_id: 1,
            slug: None,
            overlay_path: PathBuf::from("/srv/overlay"),
            controller: ControllerKind::Amp,
            instance: Some("ATM10".to_string()),
            command: None,
        };
        assert_eq!(controller_description(&amp), "amp (instance ATM10)");

        let command = ProfileDraft {
            controller: ControllerKind::Command,
            instance: None,
            command: Some(CommandConfig {
                status: vec!["pgrep".to_string()],
                stop: vec!["stop".to_string()],
                start: vec!["start".to_string()],
                timeout_ms: None,
            }),
            ..amp
        };
        assert_eq!(controller_description(&command), "command");
    }
}
