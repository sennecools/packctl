//! Filesystem primitives used across the updater.
//!
//! All archive-provided, provider-provided, configuration-provided, and
//! overlay-provided paths are treated as untrusted and must pass through the
//! path safety helpers before use.

pub mod copy;
pub mod extract;
pub mod hashing;
pub mod paths;
