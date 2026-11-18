//! Command-line interface.
//!
//! CLI modules translate user input into domain operations. They must not
//! contain the actual update algorithm; all decisions live in the core.

pub mod adopt;
pub mod apikey;
pub mod completions;
pub mod create;
pub mod doctor;
pub mod plan;
pub(crate) mod prompt;
pub mod rollback;
pub mod status;
pub mod update;
pub mod validate;
pub mod versions;

use std::path::PathBuf;
use std::sync::LazyLock;

use clap::{Parser, Subcommand};

use crate::config::profile::{ControllerKind, ProviderKind};
use crate::core::adopt::{ExtraConfigPolicy, ModifiedPolicy};
use crate::error::Result;

/// The `--version` string: the package version, plus `+<sha7>` when the
/// build sets `PACKCTL_BUILD_SHA` (the rolling release does).
static VERSION: LazyLock<String> =
    LazyLock::new(|| version_string(option_env!("PACKCTL_BUILD_SHA")));

/// The `--version` output with the build target, which release builds name
/// in `PACKCTL_BUILD_TARGET`.
static LONG_VERSION: LazyLock<String> =
    LazyLock::new(|| long_version_string(&VERSION, option_env!("PACKCTL_BUILD_TARGET")));

/// `CARGO_PKG_VERSION`, plus `+` and the first seven characters of `sha` when
/// it is set and not blank.
fn version_string(sha: Option<&str>) -> String {
    let version = env!("CARGO_PKG_VERSION");
    match sha.map(str::trim).filter(|sha| !sha.is_empty()) {
        Some(sha) => format!("{version}+{}", sha.chars().take(7).collect::<String>()),
        None => version.to_string(),
    }
}

/// `version` followed by the build target, or `unknown target`.
fn long_version_string(version: &str, target: Option<&str>) -> String {
    let target = target
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .unwrap_or("unknown target");
    format!("{version} ({target})")
}

/// Help for the optional SERVER argument of the single-server commands.
const SERVER_HELP: &str = "Server profile name (default: .packctl.toml in the current directory)";

/// Long help for `packctl completions`.
const COMPLETIONS_LONG_HELP: &str = "\
Print a shell completion script (bash, zsh, fish, powershell, elvish)

Write the script where your shell loads completions, for example:

  packctl completions bash > ~/.local/share/bash-completion/completions/packctl
  packctl completions zsh > \"${fpath[1]}/_packctl\"";

#[derive(Parser)]
#[command(
    name = "packctl",
    version = VERSION.as_str(),
    long_version = LONG_VERSION.as_str(),
    about = "Safely update self-hosted Minecraft modpack servers",
    after_help = "Docs: https://github.com/sennecools/packctl#readme",
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
        /// Modpack URL, project ID, or slug (CurseForge or Modrinth)
        #[arg(long)]
        source: Option<String>,
        /// Pack provider (when omitted: detected from a CurseForge or Modrinth URL
        /// or a .zip/.mrpack path in --source; a bare project ID or slug means
        /// curseforge)
        #[arg(long, value_enum, ignore_case = true)]
        provider: Option<ProviderKind>,
        /// Local archive path (.zip or .mrpack file, or a directory of them) for --provider local;
        /// defaults to <server root>/packs
        #[arg(long)]
        archive: Option<PathBuf>,
        /// CurseForge API key to store (encrypted) with the new profile
        #[arg(long)]
        apikey: Option<String>,
        /// Server root directory (defaults to the current directory)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Overlay directory (relative paths are resolved against the server
        /// root; default <root>/overlay)
        #[arg(long)]
        overlay: Option<PathBuf>,
        /// Server controller
        #[arg(long, value_enum, ignore_case = true)]
        controller: Option<ControllerKind>,
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
        /// The server already runs this pack version (or "detect"): show what
        /// adopting it would do after the profile is written
        #[arg(long, value_name = "VERSION|detect")]
        adopt: Option<String>,
    },
    /// Record a hand-installed server as a pack version without changing it
    ///
    /// Compares every file in the pack's folders with the given version:
    /// files that match are recorded, edited files and added mods go to the
    /// overlay, and added configs, runtime data and removed or disabled mods
    /// get .packctlignore rules. Nothing is written without --apply, and
    /// 'packctl rollback' undoes an applied adoption.
    #[command(group(
        clap::ArgGroup::new("target")
            .required(true)
            .args(["as_version", "detect"])
    ))]
    Adopt {
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
        /// Version the server currently runs (name, number or id)
        #[arg(long = "as", value_name = "VERSION")]
        as_version: Option<String>,
        /// Guess the version from the jars in mods/ (local and Modrinth packs only)
        #[arg(long)]
        detect: bool,
        /// Write the overlay, .packctlignore and state (default: dry run)
        #[arg(long)]
        apply: bool,
        /// Do not ask for confirmation with --apply
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print the report as JSON
        #[arg(long, short = 'j')]
        json: bool,
        /// List every file
        #[arg(long, short = 'v')]
        verbose: bool,
        /// What to do with pack files you edited
        #[arg(long, value_enum, default_value_t = ModifiedPolicy::Overlay)]
        modified: ModifiedPolicy,
        /// What to do with config files you added
        #[arg(long = "extra-configs", value_enum, default_value_t = ExtraConfigPolicy::Ignore)]
        extra_configs: ExtraConfigPolicy,
        /// Keep files the pack ships but the server lacks out of future updates
        #[arg(long)]
        keep_removed: bool,
        /// Adopt even when state exists, most pack files are missing, or the
        /// server is running
        #[arg(long)]
        force: bool,
    },
    /// Show the current state of a server
    Status {
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
        /// Print the status as JSON
        #[arg(long, short = 'j')]
        json: bool,
    },
    /// Store, show, or remove the encrypted CurseForge API key for a server
    ///
    /// Without --set, --stdin, --show or --remove, the key is read from a
    /// hidden prompt.
    Apikey {
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
        /// Store this API key (it ends up in shell history and process
        /// lists; prefer --stdin or the prompt)
        #[arg(long, value_name = "KEY", conflicts_with = "remove")]
        set: Option<String>,
        /// Read the API key to store from one line of standard input
        #[arg(long, conflicts_with_all = ["set", "remove"])]
        stdin: bool,
        /// Show where the API key comes from and its last four characters
        #[arg(long, conflicts_with_all = ["set", "remove", "stdin"])]
        show: bool,
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
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
        /// Print versions as JSON
        #[arg(long, short = 'j')]
        json: bool,
    },
    /// Show what an update would do without changing anything
    Plan {
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
        /// Target version name or id (prefer --to when using a local .packctl.toml)
        version: Option<String>,
        /// Target version: an id, a name, or a unique part of a name such as
        /// 1.2.0 (defaults to the latest)
        #[arg(long = "to", value_name = "VERSION", conflicts_with = "version")]
        to: Option<String>,
        /// Print the full file-level plan
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Print the plan as JSON (skips the server checks)
        #[arg(long, short = 'j')]
        json: bool,
    },
    /// Apply an update
    Update {
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
        /// Target version name or id (prefer --to when using a local .packctl.toml)
        version: Option<String>,
        /// Target version: an id, a name, or a unique part of a name such as
        /// 1.2.0 (defaults to interactive selection)
        #[arg(long = "to", value_name = "VERSION", conflicts_with = "version")]
        to: Option<String>,
        /// Do not prompt; fail if confirmation would be required
        #[arg(long, short = 'n')]
        non_interactive: bool,
        /// Print the full file-level plan before applying
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Apply the plan even when it deletes or overwrites files packctl
        /// never installed, or writes overlay files over live files that
        /// changed outside packctl
        #[arg(long)]
        force_sync: bool,
    },
    /// Roll back to the previous successful version
    Rollback {
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
    },
    /// Check a server for common problems
    Doctor {
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
    },
    /// Validate the installed server against its recorded state
    Validate {
        #[arg(help = SERVER_HELP)]
        server: Option<String>,
    },
    /// Print a shell completion script (bash, zsh, fish, powershell, elvish)
    #[command(long_about = COMPLETIONS_LONG_HELP)]
    Completions {
        /// The shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
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
            adopt,
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
                adopt,
            })
            .await
        }
        Command::Adopt {
            server,
            as_version,
            detect,
            apply,
            yes,
            json,
            verbose,
            modified,
            extra_configs,
            keep_removed,
            force,
        } => {
            adopt::run(
                server.as_deref(),
                adopt::AdoptArgs {
                    version: as_version,
                    detect,
                    apply,
                    yes,
                    json,
                    verbose,
                    modified,
                    extra_configs,
                    keep_removed,
                    force,
                },
            )
            .await
        }
        Command::Status { server, json } => status::run(server.as_deref(), json).await,
        Command::Apikey {
            server,
            set,
            stdin,
            show,
            remove,
            global,
        } => {
            apikey::run(
                server.as_deref(),
                apikey::ApikeyArgs {
                    set,
                    stdin,
                    show,
                    remove,
                    global,
                },
            )
            .await
        }
        Command::Versions { server, json } => versions::run(server.as_deref(), json).await,
        Command::Plan {
            server,
            version,
            to,
            verbose,
            json,
        } => plan::run(server.as_deref(), to.or(version).as_deref(), verbose, json).await,
        Command::Update {
            server,
            version,
            to,
            non_interactive,
            verbose,
            force_sync,
        } => {
            update::run(
                server.as_deref(),
                to.or(version).as_deref(),
                update::UpdateFlags {
                    non_interactive,
                    verbose,
                    force_sync,
                },
            )
            .await
        }
        Command::Rollback { server } => rollback::run(server.as_deref()).await,
        Command::Doctor { server } => doctor::run(server.as_deref()).await,
        Command::Validate { server } => validate::run(server.as_deref()).await,
        Command::Completions { shell } => completions::run(shell),
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
            to,
            non_interactive,
            verbose,
            force_sync,
        } = parse(&["packctl", "update", "atm10", "1.2.3", "-n", "-v"])
        else {
            panic!("expected the update command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert_eq!(version.as_deref(), Some("1.2.3"));
        assert_eq!(to, None);
        assert!(non_interactive);
        assert!(verbose);
        assert!(!force_sync);
    }

    #[test]
    fn update_defaults_to_interactive_and_no_explicit_target() {
        let Command::Update {
            server,
            version,
            to,
            non_interactive,
            verbose,
            force_sync,
        } = parse(&["packctl", "update"])
        else {
            panic!("expected the update command");
        };
        assert_eq!(server, None);
        assert_eq!(version, None);
        assert_eq!(to, None);
        assert!(!non_interactive);
        assert!(!verbose);
        assert!(!force_sync);
    }

    #[test]
    fn update_accepts_to_and_force_sync() {
        let Command::Update {
            server,
            to,
            force_sync,
            ..
        } = parse(&["packctl", "update", "--to", "1.2.0", "--force-sync", "-n"])
        else {
            panic!("expected the update command");
        };
        assert_eq!(server, None);
        assert_eq!(to.as_deref(), Some("1.2.0"));
        assert!(force_sync);
    }

    #[test]
    fn update_to_conflicts_with_positional_version() {
        assert_eq!(
            parse_error(&["packctl", "update", "atm10", "1.2.3", "--to", "1.2.4"]),
            ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn plan_parses_long_verbose_and_leaves_version_unset() {
        let Command::Plan {
            server,
            version,
            to,
            verbose,
            json,
        } = parse(&["packctl", "plan", "atm10", "--verbose"])
        else {
            panic!("expected the plan command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert_eq!(version, None);
        assert_eq!(to, None);
        assert!(verbose);
        assert!(!json);
    }

    #[test]
    fn plan_json_flag_parses() {
        let Command::Plan { server, json, .. } = parse(&["packctl", "plan", "atm10", "--json"])
        else {
            panic!("expected the plan command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert!(json);
        let Command::Plan { json, to, .. } = parse(&["packctl", "plan", "-j", "--to", "8.2"])
        else {
            panic!("expected the plan command");
        };
        assert!(json);
        assert_eq!(to.as_deref(), Some("8.2"));
    }

    #[test]
    fn status_json_flag_parses() {
        let Command::Status { server, json } = parse(&["packctl", "status", "atm10", "--json"])
        else {
            panic!("expected the status command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert!(json);
        let Command::Status { server, json } = parse(&["packctl", "status", "-j"]) else {
            panic!("expected the status command");
        };
        assert_eq!(server, None);
        assert!(json);
        let Command::Status { json, .. } = parse(&["packctl", "status"]) else {
            panic!("expected the status command");
        };
        assert!(!json);
    }

    #[test]
    fn plan_accepts_to_flag() {
        // With a local .packctl.toml there is no server name, so --to is the
        // only way to pick a version.
        let Command::Plan {
            server,
            version,
            to,
            ..
        } = parse(&["packctl", "plan", "--to", "ATM8 1.1.0"])
        else {
            panic!("expected the plan command");
        };
        assert_eq!(server, None);
        assert_eq!(version, None);
        assert_eq!(to.as_deref(), Some("ATM8 1.1.0"));

        let Command::Plan { server, to, .. } = parse(&["packctl", "plan", "atm10", "--to", "5002"])
        else {
            panic!("expected the plan command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert_eq!(to.as_deref(), Some("5002"));
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
            adopt,
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
        assert_eq!(provider, Some(ProviderKind::Local));
        assert_eq!(archive, Some(PathBuf::from("p.zip")));
        assert_eq!(controller, Some(ControllerKind::Command));
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
        assert_eq!(adopt, None);
    }

    fn create_provider(value: &str) -> Option<ProviderKind> {
        match parse(&["packctl", "create", "x", "--provider", value]) {
            Command::Create { provider, .. } => provider,
            _ => panic!("expected the create command"),
        }
    }

    fn create_controller(value: &str) -> Option<ControllerKind> {
        match parse(&["packctl", "create", "x", "--controller", value]) {
            Command::Create { controller, .. } => controller,
            _ => panic!("expected the create command"),
        }
    }

    #[test]
    fn provider_flag_is_value_enum() {
        assert_eq!(
            create_provider("curseforge"),
            Some(ProviderKind::CurseForge)
        );
        assert_eq!(create_provider("modrinth"), Some(ProviderKind::Modrinth));
        assert_eq!(create_provider("local"), Some(ProviderKind::Local));
        assert_eq!(
            create_provider("CurseForge"),
            Some(ProviderKind::CurseForge)
        );
        assert_eq!(
            parse_error(&["packctl", "create", "x", "--provider", "ftb"]),
            ErrorKind::InvalidValue
        );
        // The derived kebab-case spelling is not accepted.
        assert_eq!(
            parse_error(&["packctl", "create", "x", "--provider", "curse-forge"]),
            ErrorKind::InvalidValue
        );
        let Command::Create { provider, .. } = parse(&["packctl", "create", "x"]) else {
            panic!("expected the create command");
        };
        assert_eq!(provider, None);
    }

    #[test]
    fn controller_flag_is_value_enum() {
        assert_eq!(create_controller("amp"), Some(ControllerKind::Amp));
        assert_eq!(create_controller("command"), Some(ControllerKind::Command));
        assert_eq!(create_controller("COMMAND"), Some(ControllerKind::Command));
        assert_eq!(
            parse_error(&["packctl", "create", "x", "--controller", "docker"]),
            ErrorKind::InvalidValue
        );
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
            stdin,
            show,
            remove,
            global,
        } = parse(&["packctl", "apikey", "x", "--set", "k", "--global"])
        else {
            panic!("expected the apikey command");
        };
        assert_eq!(server.as_deref(), Some("x"));
        assert_eq!(set.as_deref(), Some("k"));
        assert!(!stdin && !show && !remove);
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
    fn apikey_set_conflicts_with_remove() {
        assert_eq!(
            parse_error(&["packctl", "apikey", "--set", "k", "--remove"]),
            ErrorKind::ArgumentConflict
        );
        assert_eq!(
            parse_error(&["packctl", "apikey", "--stdin", "--remove"]),
            ErrorKind::ArgumentConflict
        );
        assert_eq!(
            parse_error(&["packctl", "apikey", "--stdin", "--set", "k"]),
            ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn apikey_show_conflicts_with_set() {
        for args in [
            &["packctl", "apikey", "--show", "--set", "k"][..],
            &["packctl", "apikey", "--show", "--remove"][..],
            &["packctl", "apikey", "--show", "--stdin"][..],
        ] {
            assert_eq!(parse_error(args), ErrorKind::ArgumentConflict, "{args:?}");
        }
        // --show combines with --global.
        let Command::Apikey { show, global, .. } =
            parse(&["packctl", "apikey", "--show", "--global"])
        else {
            panic!("expected the apikey command");
        };
        assert!(show && global);
    }

    #[test]
    fn apikey_stdin_flag_parses() {
        let Command::Apikey {
            server,
            set,
            stdin,
            show,
            remove,
            global,
        } = parse(&["packctl", "apikey", "atm10", "--stdin"])
        else {
            panic!("expected the apikey command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert_eq!(set, None);
        assert!(stdin);
        assert!(!show && !remove && !global);
    }

    #[test]
    fn completions_parses_each_shell() {
        use clap_complete::Shell;
        for (name, expected) in [
            ("bash", Shell::Bash),
            ("zsh", Shell::Zsh),
            ("fish", Shell::Fish),
            ("powershell", Shell::PowerShell),
            ("elvish", Shell::Elvish),
        ] {
            let Command::Completions { shell } = parse(&["packctl", "completions", name]) else {
                panic!("expected the completions command");
            };
            assert_eq!(shell, expected);
        }
        assert_eq!(
            parse_error(&["packctl", "completions", "tcsh"]),
            ErrorKind::InvalidValue
        );
        assert_eq!(
            parse_error(&["packctl", "completions"]),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn completions_bash_output_mentions_adopt_and_plan() {
        let mut out: Vec<u8> = Vec::new();
        completions::write_completions(clap_complete::Shell::Bash, &mut out);
        let script = String::from_utf8(out).unwrap();
        assert!(script.contains("packctl"), "{script}");
        assert!(script.contains("adopt"));
        assert!(script.contains("plan"));
        assert!(script.contains("--force-sync"));
    }

    #[test]
    fn version_string_without_build_sha_is_pkg_version() {
        assert_eq!(version_string(None), env!("CARGO_PKG_VERSION"));
        assert_eq!(version_string(Some("")), env!("CARGO_PKG_VERSION"));
        assert_eq!(version_string(Some("  ")), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn with_sha_appends_short_sha() {
        assert_eq!(
            version_string(Some("1373d0c5e8a9b2f4")),
            format!("{}+1373d0c", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            version_string(Some("abc")),
            format!("{}+abc", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            long_version_string("0.1.0+1373d0c", Some("x86_64-unknown-linux-musl")),
            "0.1.0+1373d0c (x86_64-unknown-linux-musl)"
        );
        assert_eq!(long_version_string("0.1.0", None), "0.1.0 (unknown target)");
    }

    #[test]
    fn version_flag_prints_the_version() {
        let err = Cli::try_parse_from(["packctl", "--version"])
            .err()
            .expect("--version exits early");
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(VERSION.as_str()), "{err}");
    }

    #[test]
    fn server_help_is_shared_by_single_server_commands() {
        let command = Cli::command();
        for name in [
            "adopt", "status", "apikey", "versions", "plan", "update", "rollback", "doctor",
            "validate",
        ] {
            let sub = command.find_subcommand(name).unwrap();
            let server = sub
                .get_arguments()
                .find(|arg| arg.get_id() == "server")
                .unwrap_or_else(|| panic!("{name} has a server argument"));
            assert_eq!(
                server.get_help().map(ToString::to_string).as_deref(),
                Some(SERVER_HELP),
                "{name}"
            );
        }
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
            Command::Status { server: None, .. }
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

    #[test]
    fn adopt_requires_as_or_detect() {
        assert_eq!(
            parse_error(&["packctl", "adopt"]),
            ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            parse_error(&["packctl", "adopt", "atm10", "--apply"]),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn adopt_as_conflicts_with_detect() {
        assert_eq!(
            parse_error(&["packctl", "adopt", "--as", "8.2", "--detect"]),
            ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn adopt_parses_defaults_and_flags() {
        let Command::Adopt {
            server,
            as_version,
            detect,
            apply,
            yes,
            json,
            verbose,
            modified,
            extra_configs,
            keep_removed,
            force,
        } = parse(&["packctl", "adopt", "atm10", "--as", "8.2"])
        else {
            panic!("expected the adopt command");
        };
        assert_eq!(server.as_deref(), Some("atm10"));
        assert_eq!(as_version.as_deref(), Some("8.2"));
        assert!(!detect && !apply && !yes && !json && !verbose && !keep_removed && !force);
        assert_eq!(modified, ModifiedPolicy::Overlay);
        assert_eq!(extra_configs, ExtraConfigPolicy::Ignore);

        let Command::Adopt {
            detect,
            apply,
            yes,
            json,
            verbose,
            modified,
            extra_configs,
            keep_removed,
            force,
            ..
        } = parse(&[
            "packctl",
            "adopt",
            "--detect",
            "--apply",
            "-y",
            "-j",
            "-v",
            "--modified",
            "upstream",
            "--extra-configs",
            "overlay",
            "--keep-removed",
            "--force",
        ])
        else {
            panic!("expected the adopt command");
        };
        assert!(detect && apply && yes && json && verbose && keep_removed && force);
        assert_eq!(modified, ModifiedPolicy::Upstream);
        assert_eq!(extra_configs, ExtraConfigPolicy::Overlay);
        assert_eq!(
            parse_error(&["packctl", "adopt", "--as", "1", "--modified", "maybe"]),
            ErrorKind::InvalidValue
        );
    }

    #[test]
    fn create_accepts_adopt() {
        let Command::Create { adopt, .. } =
            parse(&["packctl", "create", "x", "-n", "--adopt", "detect"])
        else {
            panic!("expected the create command");
        };
        assert_eq!(adopt.as_deref(), Some("detect"));
    }
}
