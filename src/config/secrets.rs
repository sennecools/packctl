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
use std::io::{self, Read};
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;

use crate::error::{PackError, Result};

const KEY_FILE_NAME: &str = ".key";
/// Version prefix on stored blobs so the format can evolve.
const BLOB_PREFIX: &str = "v1:";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Path of the per-user master key file.
pub fn key_file_path() -> Result<std::path::PathBuf> {
    Ok(crate::config::profile::profile_dir()?.join(KEY_FILE_NAME))
}

/// Loads the per-user master key, creating a fresh random one on first use.
pub fn load_or_create_master_key() -> Result<[u8; KEY_LEN]> {
    let path = key_file_path()?;
    match fs::read(&path) {
        Ok(bytes) => bytes_as_key(bytes, &path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let key = random_bytes(KEY_LEN)?;
            write_key_file(&path, &key)?;
            Ok(bytes_as_key(key, &path)?)
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

fn write_key_file(path: &Path, key: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| PackError::io(format!("create '{}'", parent.display()), err))?;
    }
    fs::write(path, key)
        .map_err(|err| PackError::io(format!("write '{}'", path.display()), err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| {
            PackError::io(format!("set permissions on '{}'", path.display()), err)
        })?;
    }
    Ok(())
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
