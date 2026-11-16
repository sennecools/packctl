//! Command-line interface.
//!
//! CLI modules translate user input into domain operations. They must not
//! contain the actual update algorithm; all decisions live in the core.

pub mod apikey;
pub mod create;
pub mod doctor;
pub mod plan;
pub mod rollback;
pub mod status;
pub mod update;
pub mod validate;
pub mod versions;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::error::Result;

#[derive(Parser)]
#[command(
    name = "packctl",
    version,
    about = "Safely update self-hosted Minecraft modpack servers",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
// The `create` variant carries many optional flags; that is inherent to a
// command-line interface and not worth boxing every variant to appease the
// size lint.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// List configured server profiles
    List,
    /// Create a new server profile interactively
    Create {
        /// Server profile name (defaults to the current directory name)
        name: Option<String>,
        /// Do not prompt; provide every value as an option
        #[arg(long, short = 'n')]
        non_interactive: bool,
        /// Overwrite the profile if it already exists
        #[arg(long, short = 'f')]
        force: bool,
        /// Write the profile into the global profile directory instead of the
        /// current directory
        #[arg(long)]
        global: bool,
        /// CurseForge modpack URL, project ID, or slug (defaults to a prompt)
        #[arg(long)]
        source: Option<String>,
        /// Pack provider kind: "curseforge" (default) or "local"
        #[arg(long)]
        provider: Option<String>,
        /// Local archive path (zip file or directory of zips) for --provider local;
        /// defaults to <server root>/packs
        #[arg(long)]
        archive: Option<PathBuf>,
        /// CurseForge API key to store (encrypted) with the new profile
        #[arg(long)]
        apikey: Option<String>,
        /// Server root directory (defaults to the current directory)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Overlay directory (defaults to <root>/overlay)
        #[arg(long)]
        overlay: Option<PathBuf>,
        /// Controller kind: "amp" or "command"
        #[arg(long)]
        controller: Option<String>,
        /// AMP instance name (for --controller amp)
        #[arg(long)]
        instance: Option<String>,
        /// Status command (space-separated argv) for a command controller
        #[arg(long)]
        status: Option<String>,
        /// Stop command (space-separated argv) for a command controller
        #[arg(long)]
        stop: Option<String>,
        /// Start command (space-separated argv) for a command controller
        #[arg(long)]
        start: Option<String>,
        /// Timeout in milliseconds for command-controller invocations
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Show the current state of a server
    Status {
        /// Server profile name (or use .packctl.toml in the current directory)
        server: Option<String>,
    },
    /// Store, show, or remove the encrypted CurseForge API key for a server
    Apikey {
        /// Server profile name (or use .packctl.toml in the current directory)
        server: Option<String>,
        /// Set the API key to this value (otherwise prompted)
        #[arg(long)]
        set: Option<String>,
        /// Remove the stored API key
        #[arg(long)]
        remove: bool,
        /// Manage the shared, machine-wide key used by every profile instead of
        /// a per-server key
        #[arg(long)]
        global: bool,
    },
    /// List available upstream versions for a server
    Versions {
        /// Server profile name (or use .packctl.toml in the current directory)
        server: Option<String>,
        /// Print versions as JSON
        #[arg(long, short = 'j')]
        json: bool,
    },
    /// Show what an update would do without changing anything
    Plan {
        /// Server profile name (or use .packctl.toml in the current directory)
        server: Option<String>,
        /// Target version (defaults to the latest)
        version: Option<String>,
        /// Print the full file-level plan
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Apply an update
    Update {
        /// Server profile name (or use .packctl.toml in the current directory)
        server: Option<String>,
        /// Target version (defaults to interactive selection)
        version: Option<String>,
        /// Do not prompt; fail if confirmation would be required
        #[arg(long, short = 'n')]
        non_interactive: bool,
        /// Print the full file-level plan before applying
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Roll back to the previous successful version
    Rollback {
        /// Server profile name (or use .packctl.toml in the current directory)
        server: Option<String>,
    },
    /// Check a server for common problems
    Doctor {
        /// Server profile name (or use .packctl.toml in the current directory)
        server: Option<String>,
    },
    /// Validate the installed server against its recorded state
    Validate {
        /// Server profile name (or use .packctl.toml in the current directory)
        server: Option<String>,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List => crate::config::profile::list_cli(),
        Command::Create {
            name,
            non_interactive,
            force,
            global,
            source,
            provider,
            archive,
            apikey,
            root,
            overlay,
            controller,
            instance,
            status,
            stop,
            start,
            timeout_ms,
        } => {
            create::run(create::CreateArgs {
                name,
                non_interactive,
                force,
                global,
                source,
                provider,
                archive,
                apikey,
                root,
                overlay,
                controller,
                instance,
                status,
                stop,
                start,
                timeout_ms,
            })
            .await
        }
        Command::Status { server } => status::run(server.as_deref()).await,
        Command::Apikey {
            server,
            set,
            remove,
            global,
        } => apikey::run(server.as_deref(), set, remove, global).await,
        Command::Versions { server, json } => versions::run(server.as_deref(), json).await,
        Command::Plan {
            server,
            version,
            verbose,
        } => plan::run(server.as_deref(), version.as_deref(), verbose).await,
        Command::Update {
            server,
            version,
            non_interactive,
            verbose,
        } => {
            update::run(
                server.as_deref(),
                version.as_deref(),
                non_interactive,
                verbose,
            )
            .await
        }
        Command::Rollback { server } => rollback::run(server.as_deref()).await,
        Command::Doctor { server } => doctor::run(server.as_deref()).await,
        Command::Validate { server } => validate::run(server.as_deref()).await,
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    use super::*;

    fn parse(args: &[&str]) -> Command {
        match Cli::try_parse_from(args) {
            Ok(cli) => cli.command,
            Err(err) => panic!("{args:?} should parse: {err}"),
        }
    }

    fn parse_error(args: &[&str]) -> ErrorKind {
        match Cli::try_parse_from(args) {
            Ok(_) => panic!("{args:?} should be rejected"),
            Err(err) => err.kind(),
        }
    }

    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn list_takes_no_arguments() {
        assert!(matches!(parse(&["packctl", "list"]), Command::List));
        assert_eq!(
            parse_error(&["packctl", "list", "atm10"]),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn update_parses_server_version_and_short_flags() {
        let Command::Update {
            server,
            version,
            non_interactive,
            verbose,
        } = parse(&["packctl", "update", "atm10", "1.2.3", "-n", "-v"])
        else {
            panic!("expected the update command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert_eq!(version.as_deref(), Some("1.2.3"));
        assert!(non_interactive);
        assert!(verbose);
    }

    #[test]
    fn update_defaults_to_interactive_and_no_explicit_target() {
        let Command::Update {
            server,
            version,
            non_interactive,
            verbose,
        } = parse(&["packctl", "update"])
        else {
            panic!("expected the update command");
        };
        assert_eq!(server, None);
        assert_eq!(version, None);
        assert!(!non_interactive);
        assert!(!verbose);
    }

    #[test]
    fn plan_parses_long_verbose_and_leaves_version_unset() {
        let Command::Plan {
            server,
            version,
            verbose,
        } = parse(&["packctl", "plan", "atm10", "--verbose"])
        else {
            panic!("expected the plan command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert_eq!(version, None);
        assert!(verbose);
    }

    #[test]
    fn plan_has_no_non_interactive_flag() {
        // `plan` never mutates, so the confirmation flag belongs to `update`
        // only.
        assert_eq!(
            parse_error(&["packctl", "plan", "atm10", "-n"]),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn create_parses_local_provider_with_command_controller() {
        let Command::Create {
            name,
            non_interactive,
            force,
            global,
            source,
            provider,
            archive,
            apikey,
            root,
            overlay,
            controller,
            instance,
            status,
            stop,
            start,
            timeout_ms,
        } = parse(&[
            "packctl",
            "create",
            "x",
            "-n",
            "--provider",
            "local",
            "--archive",
            "p.zip",
            "--controller",
            "command",
            "--status",
            "systemctl is-active mc",
            "--timeout-ms",
            "500",
        ])
        else {
            panic!("expected the create command");
        };
        assert_eq!(name.as_deref(), Some("x"));
        assert!(non_interactive);
        assert_eq!(provider.as_deref(), Some("local"));
        assert_eq!(archive, Some(PathBuf::from("p.zip")));
        assert_eq!(controller.as_deref(), Some("command"));
        // The command line is kept as one string; splitting it into argv is
        // the create command's job.
        assert_eq!(status.as_deref(), Some("systemctl is-active mc"));
        assert_eq!(timeout_ms, Some(500));

        // Everything that was not passed stays unset so the wizard can tell
        // "not given" apart from a value.
        assert!(!force);
        assert!(!global);
        assert_eq!(source, None);
        assert_eq!(apikey, None);
        assert_eq!(root, None);
        assert_eq!(overlay, None);
        assert_eq!(instance, None);
        assert_eq!(stop, None);
        assert_eq!(start, None);
    }

    #[test]
    fn create_rejects_a_non_numeric_or_negative_timeout() {
        assert_eq!(
            parse_error(&["packctl", "create", "x", "--timeout-ms", "abc"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            parse_error(&["packctl", "create", "x", "--timeout-ms=-1"]),
            ErrorKind::ValueValidation
        );
    }

    #[test]
    fn apikey_parses_set_and_global() {
        let Command::Apikey {
            server,
            set,
            remove,
            global,
        } = parse(&["packctl", "apikey", "x", "--set", "k", "--global"])
        else {
            panic!("expected the apikey command");
        };
        assert_eq!(server.as_deref(), Some("x"));
        assert_eq!(set.as_deref(), Some("k"));
        assert!(!remove);
        assert!(global);
    }

    #[test]
    fn apikey_set_requires_a_value() {
        assert_eq!(
            parse_error(&["packctl", "apikey", "x", "--set"]),
            ErrorKind::InvalidValue
        );
    }

    #[test]
    fn versions_parses_short_json_flag() {
        let Command::Versions { server, json } = parse(&["packctl", "versions", "x", "-j"]) else {
            panic!("expected the versions command");
        };
        assert_eq!(server.as_deref(), Some("x"));
        assert!(json);

        let Command::Versions { json, .. } = parse(&["packctl", "versions", "x"]) else {
            panic!("expected the versions command");
        };
        assert!(!json);
    }

    #[test]
    fn server_argument_is_optional_for_single_server_commands() {
        // These fall back to .packctl.toml in the current directory.
        assert!(matches!(
            parse(&["packctl", "rollback"]),
            Command::Rollback { server: None }
        ));
        assert!(matches!(
            parse(&["packctl", "status"]),
            Command::Status { server: None }
        ));
        assert!(matches!(
            parse(&["packctl", "doctor"]),
            Command::Doctor { server: None }
        ));
        assert!(matches!(
            parse(&["packctl", "validate"]),
            Command::Validate { server: None }
        ));

        let Command::Rollback { server } = parse(&["packctl", "rollback", "atm10"]) else {
            panic!("expected the rollback command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
    }

    #[test]
    fn rollback_does_not_accept_a_version() {
        // Rollback always restores the latest snapshot; a second positional
        // must not be silently ignored.
        assert_eq!(
            parse_error(&["packctl", "rollback", "atm10", "1.2.3"]),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn a_subcommand_is_required() {
        // `arg_required_else_help` prints help instead of running anything.
        assert_eq!(
            parse_error(&["packctl"]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert_eq!(
            parse_error(&["packctl", "bogus"]),
            ErrorKind::InvalidSubcommand
        );
    }
}
