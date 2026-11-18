//! Shared helpers for interactive prompts.
//!
//! Every command that asks a question goes through these helpers, so a
//! missing terminal and a failed prompt read are reported the same way
//! everywhere.

use std::io::IsTerminal;

use crate::error::{PackError, Result};

/// Wraps a [`dialoguer::Error`] with the operation that failed.
pub(crate) fn dialoguer_error(what: &str, err: dialoguer::Error) -> PackError {
    match err {
        dialoguer::Error::IO(source) => PackError::io(what, source),
    }
}

/// The error for a prompt that needs a terminal when stdin is not one.
/// `hint` tells the user how to proceed without a prompt.
pub(crate) fn not_a_tty_error(hint: &str) -> PackError {
    PackError::Config(format!("stdin is not a terminal; {hint}"))
}

/// Fails with [`not_a_tty_error`] unless stdin is a terminal.
pub(crate) fn require_tty(hint: &str) -> Result<()> {
    if std::io::stdin().is_terminal() {
        Ok(())
    } else {
        Err(not_a_tty_error(hint))
    }
}

/// Reads a secret without echoing it. The caller checks for a terminal
/// first (see [`require_tty`]). With `allow_empty` an empty answer is
/// returned as an empty string; otherwise the prompt repeats until the user
/// types something.
pub(crate) fn read_secret(prompt: &str, allow_empty: bool) -> Result<String> {
    dialoguer::Password::new()
        .with_prompt(prompt)
        .allow_empty_password(allow_empty)
        .interact()
        .map_err(|err| dialoguer_error("read secret", err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_a_tty_error_carries_the_hint() {
        let err = not_a_tty_error("pass --yes to continue");
        assert!(matches!(err, PackError::Config(_)));
        assert_eq!(
            err.to_string(),
            "stdin is not a terminal; pass --yes to continue"
        );
    }
}
