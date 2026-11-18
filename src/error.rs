//! Structured error types for packctl.
//!
//! Callers that need to react to specific failure modes match on variants.
//! Operational failures carry contextual wrapping near the boundary where the
//! operation is understood.

use std::path::PathBuf;

use thiserror::Error;

/// Top-level crate error.
#[derive(Error, Debug)]
pub enum PackError {
    #[error("{0}")]
    Config(String),

    #[error("{0}")]
    NotFound(String),

    #[error("unsafe path rejected: {0}")]
    UnsafePath(PathBuf),

    #[error("unsafe path component '{component}' in '{path}'")]
    UnsafePathComponent { path: PathBuf, component: String },

    #[error("{operation} failed: {source}")]
    Io {
        operation: String,
        source: std::io::Error,
    },

    #[error("{message}\n\npath: {path}")]
    Path { message: String, path: PathBuf },

    #[error("provider error: {0}")]
    Provider(String),

    #[error("controller error: {0}")]
    Controller(String),

    #[error("validation failed: {0}")]
    Validation(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("state error: {0}")]
    State(String),

    #[error("{0}")]
    Other(String),
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, PackError>;

impl PackError {
    /// Wrap an [`std::io::Error`] with the operation that failed.
    pub fn io(operation: impl Into<String>, source: std::io::Error) -> Self {
        PackError::Io {
            operation: operation.into(),
            source,
        }
    }

    /// The process exit code for this error.
    ///
    /// Configuration and parse errors (bad input, bad profile) exit with 2,
    /// validation failures (the server or plan did not pass checks) with 3,
    /// and every other failure with 1.
    pub fn exit_code(&self) -> u8 {
        match self {
            PackError::Config(_) | PackError::Parse(_) => 2,
            PackError::Validation(_) => 3,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_mapping() {
        assert_eq!(PackError::Config("bad".into()).exit_code(), 2);
        assert_eq!(PackError::Parse("bad".into()).exit_code(), 2);
        assert_eq!(
            PackError::Validation("1 error(s) found".into()).exit_code(),
            3
        );
        assert_eq!(PackError::NotFound("x".into()).exit_code(), 1);
        assert_eq!(PackError::Provider("x".into()).exit_code(), 1);
        assert_eq!(PackError::Other("x".into()).exit_code(), 1);
        assert_eq!(
            PackError::io("read", std::io::Error::other("boom")).exit_code(),
            1
        );
    }
}
