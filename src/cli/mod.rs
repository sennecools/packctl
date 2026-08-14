//! Command-line interface.
//!
//! CLI modules translate user input into domain operations. They must not
//! contain the actual update algorithm.

use crate::error::Result;

pub async fn run() -> Result<()> {
    // Wired to the full clap CLI during the CLI milestone.
    unimplemented!("CLI not yet implemented")
}
