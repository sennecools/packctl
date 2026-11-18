//! Console output that tolerates a closed pipe.
//!
//! `println!` panics when stdout is a pipe whose reader has gone away (for
//! example `packctl list | head -1`). The `out!`, `outln!`, `err!` and
//! `errln!` macros write the same text but drop it once the reader is gone,
//! so the command still runs to completion: an update is never cut short
//! halfway because nobody reads its progress any more. Any other write error
//! still panics, as `println!` does.

use std::fmt;
use std::io::{self, Write};

/// Like `print!`, ignoring a closed stdout pipe.
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::output::write_stdout(format_args!($($arg)*))
    };
}

/// Like `println!`, ignoring a closed stdout pipe.
macro_rules! outln {
    () => {
        $crate::output::write_stdout(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::output::write_stdout(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Like `eprint!`, ignoring a closed stderr pipe.
macro_rules! err {
    ($($arg:tt)*) => {
        $crate::output::write_stderr(format_args!($($arg)*))
    };
}

/// Like `eprintln!`, ignoring a closed stderr pipe.
macro_rules! errln {
    () => {
        $crate::output::write_stderr(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::output::write_stderr(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Writes to stdout (see the module docs).
pub fn write_stdout(args: fmt::Arguments<'_>) {
    if let Err(error) = write_ignoring_broken_pipe(&mut io::stdout().lock(), args) {
        panic!("failed printing to stdout: {error}");
    }
}

/// Writes to stderr (see the module docs).
pub fn write_stderr(args: fmt::Arguments<'_>) {
    if let Err(error) = write_ignoring_broken_pipe(&mut io::stderr().lock(), args) {
        panic!("failed printing to stderr: {error}");
    }
}

/// Writes `args`, treating a reader that went away as success.
fn write_ignoring_broken_pipe(out: &mut impl Write, args: fmt::Arguments<'_>) -> io::Result<()> {
    match out.write_fmt(args) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn broken_pipe_is_ignored_other_errors_are_not() {
        let mut gone = FailingWriter(io::ErrorKind::BrokenPipe);
        assert!(write_ignoring_broken_pipe(&mut gone, format_args!("a {}\n", 1)).is_ok());
        let mut full = FailingWriter(io::ErrorKind::StorageFull);
        assert!(write_ignoring_broken_pipe(&mut full, format_args!("a\n")).is_err());

        let mut buffer = Vec::new();
        write_ignoring_broken_pipe(&mut buffer, format_args!("{}-{}", "x", 2)).unwrap();
        assert_eq!(buffer, b"x-2");
    }
}
