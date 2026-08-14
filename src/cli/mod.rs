//! Command-line interface.
//!
//! CLI modules translate user input into domain operations. They must not
//! contain the actual update algorithm; all decisions live in the core.

pub mod doctor;
pub mod plan;
pub mod rollback;
pub mod status;
pub mod update;
pub mod validate;
pub mod versions;

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
pub enum Command {
    /// List configured server profiles
    List,
    /// Show the current state of a server
    Status {
        /// Server profile name
        server: String,
    },
    /// List available upstream versions for a server
    Versions {
        /// Server profile name
        server: String,
        /// Print versions as JSON
        #[arg(long, short = 'j')]
        json: bool,
    },
    /// Show what an update would do without changing anything
    Plan {
        /// Server profile name
        server: String,
        /// Target version (defaults to the latest)
        version: Option<String>,
        /// Print the full file-level plan
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Apply an update
    Update {
        /// Server profile name
        server: String,
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
        /// Server profile name
        server: String,
    },
    /// Check a server for common problems
    Doctor {
        /// Server profile name
        server: String,
    },
    /// Validate the installed server against its recorded state
    Validate {
        /// Server profile name
        server: String,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List => crate::config::profile::list_cli(),
        Command::Status { server } => status::run(&server).await,
        Command::Versions { server, json } => versions::run(&server, json).await,
        Command::Plan {
            server,
            version,
            verbose,
        } => plan::run(&server, version.as_deref(), verbose).await,
        Command::Update {
            server,
            version,
            non_interactive,
            verbose,
        } => update::run(&server, version.as_deref(), non_interactive, verbose).await,
        Command::Rollback { server } => rollback::run(&server).await,
        Command::Doctor { server } => doctor::run(&server).await,
        Command::Validate { server } => validate::run(&server).await,
    }
}
