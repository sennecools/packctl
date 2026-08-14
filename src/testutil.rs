//! Shared test helpers. Only compiled in test builds.

#![cfg(test)]

use std::ffi::{OsStr, OsString};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// Global lock guarding environment-variable mutations in tests.
///
/// Multiple test modules mutate the same environment variables (for example
/// `PACKCTL_HOME`), so they must share a single lock or run in parallel and
/// clobber each other.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Restores an environment variable to its previous value on drop.
pub struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    pub fn set(key: &'static str, value: &OsStr) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        EnvGuard { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe {
                std::env::set_var(self.key, value);
            },
            None => unsafe {
                std::env::remove_var(self.key);
            },
        }
    }
}
