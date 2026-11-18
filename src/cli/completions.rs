//! `packctl completions` — print a shell completion script.

use std::io::{self, Write};

use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::Cli;
use crate::error::{PackError, Result};

/// Writes the completion script for `shell` to stdout.
///
/// The script is rendered into memory first, because `clap_complete`
/// panics on a write error. A reader that closes the pipe early (for
/// example `packctl completions bash | head -1`) is not an error.
pub fn run(shell: Shell) -> Result<()> {
    let mut script = Vec::new();
    write_completions(shell, &mut script);
    write_ignoring_broken_pipe(&mut io::stdout().lock(), &script)
}

/// Writes the completion script for `shell` to `out`.
pub fn write_completions(shell: Shell, out: &mut dyn Write) {
    clap_complete::generate(shell, &mut Cli::command(), "packctl", out);
}

/// Writes and flushes `bytes`, treating a closed pipe as success.
fn write_ignoring_broken_pipe(out: &mut impl Write, bytes: &[u8]) -> Result<()> {
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(PackError::io("write the completion script", err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer whose reader has gone away, or that fails some other way.
    struct FailingWriter(io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn broken_pipe_is_not_an_error() {
        let mut out = FailingWriter(io::ErrorKind::BrokenPipe);
        assert!(write_ignoring_broken_pipe(&mut out, b"complete -F _packctl packctl\n").is_ok());
    }

    #[test]
    fn other_write_errors_are_reported() {
        let mut out = FailingWriter(io::ErrorKind::PermissionDenied);
        assert!(write_ignoring_broken_pipe(&mut out, b"script").is_err());
    }
}
