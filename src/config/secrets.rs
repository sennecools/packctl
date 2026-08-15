//! Encrypted at-rest storage for secrets such as the CurseForge API key.
//!
//! Secrets are encrypted with AES-256-GCM using a per-user master key stored
//! in the packctl config directory, never inside the server root. This keeps
//! an API key out of plaintext for anyone who can read server files (backups,
//! control-panel file managers, other accounts).
//!
//! Threat model: the master key file is readable only by its owner (0600), so
//! this is a defense against *file readers*, not against someone with shell
//! access as the same user or root, who can read the master key itself.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;

use crate::error::{PackError, Result};

const KEY_FILE_NAME: &str = ".key";
/// File name of the shared, machine-wide API key blob in the config directory.
///
/// Unlike a per-profile key, this one is stored once and used by every profile
/// on the machine, so a single CurseForge API key covers all servers.
const GLOBAL_KEY_FILE_NAME: &str = "apikey";
/// Version prefix on stored blobs so the format can evolve.
const BLOB_PREFIX: &str = "v1:";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Path of the per-user master key file.
pub fn key_file_path() -> Result<std::path::PathBuf> {
    Ok(crate::config::profile::profile_dir()?.join(KEY_FILE_NAME))
}

/// Path of the shared, machine-wide API key blob.
pub fn global_key_file_path() -> Result<std::path::PathBuf> {
    Ok(crate::config::profile::profile_dir()?.join(GLOBAL_KEY_FILE_NAME))
}

/// Loads the shared API key stored in the config directory, if any.
pub fn load_global_key() -> Result<Option<String>> {
    let path = global_key_file_path()?;
    match fs::read_to_string(&path) {
        Ok(blob) => decrypt_string(blob.trim()).map(Some),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(PackError::io(format!("read '{}'", path.display()), err)),
    }
}

/// Stores the shared API key in the config directory, encrypted at rest.
pub fn store_global_key(key: &str) -> Result<()> {
    let blob = encrypt_string(key.trim())?;
    let path = global_key_file_path()?;
    atomic_write_secret(&path, blob.as_bytes())
}

/// Removes the shared API key from the config directory, if present.
pub fn remove_global_key() -> Result<()> {
    let path = global_key_file_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(PackError::io(format!("remove '{}'", path.display()), err)),
    }
}

/// Loads the per-user master key, creating a fresh random one on first use.
pub fn load_or_create_master_key() -> Result<[u8; KEY_LEN]> {
    let path = key_file_path()?;
    match fs::read(&path) {
        Ok(bytes) => bytes_as_key(bytes, &path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let key = random_bytes(KEY_LEN)?;
            match write_key_file(&path, &key) {
                Ok(()) => bytes_as_key(key, &path),
                // Another process won the race. Use its key so both processes
                // encrypt secrets that can be decrypted subsequently.
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => fs::read(&path)
                    .map_err(|err| {
                        PackError::io(format!("read master key '{}'", path.display()), err)
                    })
                    .and_then(|bytes| bytes_as_key(bytes, &path)),
                Err(err) => Err(PackError::io(format!("write '{}'", path.display()), err)),
            }
        }
        Err(err) => Err(PackError::io(
            format!("read master key '{}'", path.display()),
            err,
        )),
    }
}

/// Encrypts `plain` with the per-user master key, returning a versioned,
/// base64-encoded blob suitable for storing in a profile file.
pub fn encrypt_string(plain: &str) -> Result<String> {
    let key = load_or_create_master_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).expect("key length is valid");
    let nonce = random_bytes(NONCE_LEN)?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
        .map_err(|_| PackError::Other("encryption failed".to_string()))?;

    let mut payload = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ciphertext);
    Ok(format!(
        "{BLOB_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(payload)
    ))
}

/// Decrypts a blob produced by [`encrypt_string`] with the local master key.
pub fn decrypt_string(blob: &str) -> Result<String> {
    let key = load_master_key()?;
    let payload = blob.strip_prefix(BLOB_PREFIX).ok_or_else(|| {
        PackError::Config("stored secret is not a valid packctl blob".to_string())
    })?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| PackError::Config("stored secret is corrupt".to_string()))?;
    if raw.len() < NONCE_LEN {
        return Err(PackError::Config("stored secret is corrupt".to_string()));
    }
    let (nonce, ciphertext) = raw.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("key length is valid");
    let key_path = key_file_path()?;
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| {
            PackError::Config(format!(
                "stored secret could not be decrypted with the local master key ({}); \
                 it was probably encrypted on another machine or the key file changed. \
                 Set CF_API_KEY or store the key again with 'packctl apikey'",
                key_path.display()
            ))
        })?;
    String::from_utf8(plain)
        .map_err(|_| PackError::Config("decrypted secret is not valid UTF-8".to_string()))
}

/// Loads the existing master key without creating one.
fn load_master_key() -> Result<[u8; KEY_LEN]> {
    let path = key_file_path()?;
    match fs::read(&path) {
        Ok(bytes) => bytes_as_key(bytes, &path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(PackError::Config(format!(
            "master key file '{}' does not exist; it is created the first time a secret is \
             stored (run 'packctl apikey')",
            path.display()
        ))),
        Err(err) => Err(PackError::io(
            format!("read master key '{}'", path.display()),
            err,
        )),
    }
}

fn bytes_as_key(bytes: Vec<u8>, path: &Path) -> Result<[u8; KEY_LEN]> {
    bytes.as_slice().try_into().map_err(|_| {
        PackError::Config(format!(
            "master key file '{}' is corrupt (expected {KEY_LEN} bytes); delete it to create \
             a new one",
            path.display()
        ))
    })
}

fn write_key_file(path: &Path, key: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        write_key_file_unix(path, key)
    }
    #[cfg(not(unix))]
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(key)?;
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(unix)]
fn write_key_file_unix(path: &Path, key: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "master key path has no parent directory",
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "master key path has no file name",
        )
    })?;
    for attempt in 0..100 {
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            attempt
        ));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        // Set the exact requested mode while this process exclusively owns the
        // temporary file; umask may otherwise make it more restrictive.
        let result = (|| -> io::Result<()> {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(key)?;
            file.sync_all()?;
            // Linking is an atomic create: either this complete key wins, or a
            // concurrent creator has already published its own key.
            fs::hard_link(&temporary, path)
        })();
        let _ = fs::remove_file(&temporary);
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate temporary master key file",
    ))
}

/// Fills `len` bytes from the kernel CSPRNG.
fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    let mut urandom =
        fs::File::open("/dev/urandom").map_err(|err| PackError::io("open /dev/urandom", err))?;
    urandom
        .read_exact(&mut bytes)
        .map_err(|err| PackError::io("read /dev/urandom", err))?;
    Ok(bytes)
}

/// Writes `contents` to `path` atomically (temp file + rename), private on
/// Unix.
fn atomic_write_secret(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        PackError::Config(format!(
            "secret path '{}' has no parent directory",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| PackError::io(format!("create '{}'", parent.display()), err))?;
    let file_name = path.file_name().ok_or_else(|| {
        PackError::Config(format!("secret path '{}' has no file name", path.display()))
    })?;

    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let write_result = (|| -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|err| PackError::io(format!("create '{}'", temporary.display()), err))?;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|err| PackError::io(format!("chmod '{}'", temporary.display()), err))?;
            file.write_all(contents)
                .map_err(|err| PackError::io(format!("write '{}'", temporary.display()), err))?;
            file.sync_all()
                .map_err(|err| PackError::io(format!("sync '{}'", temporary.display()), err))?;
        }
        #[cfg(not(unix))]
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)
                .map_err(|err| PackError::io(format!("create '{}'", temporary.display()), err))?;
            file.write_all(contents)
                .map_err(|err| PackError::io(format!("write '{}'", temporary.display()), err))?;
            file.sync_all()
                .map_err(|err| PackError::io(format!("sync '{}'", temporary.display()), err))?;
        }
        fs::rename(&temporary, path)
            .map_err(|err| PackError::io(format!("replace '{}'", path.display()), err))
    })();
    let _ = fs::remove_file(&temporary);
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{EnvGuard, env_lock};

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let blob = encrypt_string("my-secret-key").unwrap();
        assert!(blob.starts_with(BLOB_PREFIX));
        assert_ne!(blob, "my-secret-key");
        assert!(!blob.contains("my-secret-key"));

        assert_eq!(decrypt_string(&blob).unwrap(), "my-secret-key");
    }

    #[test]
    fn master_key_is_created_once_and_private() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let first = load_or_create_master_key().unwrap();
        let second = load_or_create_master_key().unwrap();
        assert_eq!(first, second);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = key_file_path().unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn exclusive_key_creation_preserves_existing_key() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());
        let path = key_file_path().unwrap();
        let existing = [0x42; KEY_LEN];

        write_key_file(&path, &existing).unwrap();
        let error = write_key_file(&path, &[0x24; KEY_LEN]).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(path).unwrap(), existing);
    }

    #[test]
    fn decrypt_fails_without_master_key() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let err = decrypt_string("v1:AAAA").unwrap_err();
        assert!(
            matches!(err, PackError::Config(_)),
            "expected Config error, got {err:?}"
        );
    }

    #[test]
    fn global_key_round_trips_and_removes() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        assert_eq!(load_global_key().unwrap(), None);

        store_global_key(" shared-key ").unwrap();
        assert_eq!(load_global_key().unwrap().as_deref(), Some("shared-key"));

        let blob = fs::read_to_string(global_key_file_path().unwrap()).unwrap();
        assert!(!blob.contains("shared-key"), "must not be plaintext");
        assert!(blob.starts_with(BLOB_PREFIX));

        remove_global_key().unwrap();
        assert_eq!(load_global_key().unwrap(), None);

        // Removing again is a no-op.
        remove_global_key().unwrap();
    }

    #[test]
    fn decrypt_fails_on_corrupt_blob() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let err = decrypt_string("garbage").unwrap_err();
        assert!(
            matches!(err, PackError::Config(_)),
            "expected Config error, got {err:?}"
        );
    }

    #[test]
    fn decrypt_fails_with_wrong_key() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("PACKCTL_HOME", dir.path().as_os_str());

        let blob = encrypt_string("secret").unwrap();
        // Replacing the master key must invalidate the blob.
        let new_key = vec![0x42u8; KEY_LEN];
        fs::write(key_file_path().unwrap(), new_key).unwrap();

        let err = decrypt_string(&blob).unwrap_err();
        assert!(
            matches!(err, PackError::Config(_)),
            "expected Config error, got {err:?}"
        );
    }
}
