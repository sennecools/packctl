//! Shared test helpers. Only compiled in test builds.

#![cfg(test)]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use crate::controllers::{ServerController, ServerStatus};
use crate::core::detect::{JarPrint, VersionFingerprint};
use crate::error::{PackError, Result};
use crate::fs::hashing::sha256_bytes;
use crate::providers::{
    PackProvider, PackRef, PackVersion, PreparedFile, PreparedPack, ResolvedPackVersion,
    VersionSelector, resolve_version_from_list,
};

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

/// A profile for a server at `root` with its overlay at `overlay`, a
/// CurseForge pack (project 42) and a command controller.
pub(crate) fn test_profile(root: &Path, overlay: &Path) -> crate::config::profile::ServerProfile {
    use crate::config::profile::{
        CommandConfig, ControllerKind, ControllerSection, OverlaySection, PackSection,
        SecretsSection, ServerProfile, ServerSection,
    };
    ServerProfile {
        name: "test-server".to_string(),
        server: ServerSection {
            root: root.to_path_buf(),
        },
        pack: PackSection::curseforge(42),
        overlay: OverlaySection {
            path: overlay.to_path_buf(),
        },
        controller: ControllerSection {
            kind: ControllerKind::Command,
            instance: None,
            command: Some(CommandConfig {
                status: vec!["true".to_string()],
                stop: vec!["true".to_string()],
                start: vec!["true".to_string()],
                timeout_ms: None,
            }),
        },
        secrets: SecretsSection::default(),
    }
}

/// Every regular file under `dir` with its content, sorted by relative
/// path, for "nothing changed" assertions.
pub(crate) fn tree_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let rel = entry
                .path()
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (rel, std::fs::read(entry.path()).unwrap())
        })
        .collect();
    files.sort();
    files
}

/// Writes `files` (relative path, content) under `root`.
pub(crate) fn write_files(root: &Path, files: &[(&str, &[u8])]) {
    for (rel, data) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }
}

/// A fake controller that records lifecycle calls and its current state.
#[derive(Clone)]
pub(crate) struct FakeController {
    status: Arc<Mutex<ServerStatus>>,
    log: Arc<Mutex<Vec<String>>>,
    /// A file `stop` writes, the way a mod saves data on shutdown.
    on_stop: Option<(PathBuf, Vec<u8>)>,
}

impl FakeController {
    /// A controller whose server is running.
    pub(crate) fn new() -> Self {
        FakeController {
            status: Arc::new(Mutex::new(ServerStatus::Running)),
            log: Arc::new(Mutex::new(Vec::new())),
            on_stop: None,
        }
    }

    /// A running server that writes `data` to `path` when it stops.
    pub(crate) fn writing_on_stop(path: PathBuf, data: &[u8]) -> Self {
        let mut controller = Self::new();
        controller.on_stop = Some((path, data.to_vec()));
        controller
    }

    /// A controller whose server is stopped.
    pub(crate) fn stopped() -> Self {
        let controller = Self::new();
        *controller.status.lock().unwrap() = ServerStatus::Stopped;
        controller
    }

    /// The `stop`/`start` calls made so far.
    pub(crate) fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    pub(crate) fn status_value(&self) -> ServerStatus {
        *self.status.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl ServerController for FakeController {
    async fn status(&self) -> Result<ServerStatus> {
        Ok(*self.status.lock().unwrap())
    }

    async fn stop(&self) -> Result<()> {
        self.log.lock().unwrap().push("stop".to_string());
        *self.status.lock().unwrap() = ServerStatus::Stopped;
        if let Some((path, data)) = &self.on_stop {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        }
        Ok(())
    }

    async fn start(&self) -> Result<()> {
        self.log.lock().unwrap().push("start".to_string());
        *self.status.lock().unwrap() = ServerStatus::Running;
        Ok(())
    }
}

/// `(id, name, files)` of one fake version; files are `(path, content)`.
pub(crate) type FakeVersion = (String, String, Vec<(String, Vec<u8>)>);

/// A borrowed [`FakeVersion`] for [`FakeProvider::new`].
pub(crate) type FakeVersionRef<'a> = (&'a str, &'a str, &'a [(&'a str, &'a [u8])]);

/// A provider with scripted versions (newest first), each staging fixed
/// file contents. It resolves selectors like a real provider and
/// fingerprints the jars each version puts into `mods/` (size and CRC-32).
pub(crate) struct FakeProvider {
    pub versions: Vec<FakeVersion>,
}

impl FakeProvider {
    /// Versions from borrowed fixtures, newest first.
    pub(crate) fn new(versions: &[FakeVersionRef<'_>]) -> Self {
        FakeProvider {
            versions: versions
                .iter()
                .map(|(id, name, files)| {
                    (
                        id.to_string(),
                        name.to_string(),
                        files
                            .iter()
                            .map(|(rel, data)| (rel.to_string(), data.to_vec()))
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    /// One version, [`FakeProvider::version`], staging `mods/upstream.jar`
    /// and `config/upstream.toml`.
    pub(crate) fn single() -> Self {
        Self::new(&[(
            "v2",
            "2.0",
            &[
                ("mods/upstream.jar", b"UP".as_slice()),
                ("config/upstream.toml", b"TOML".as_slice()),
            ],
        )])
    }

    /// Two versions: `2.0` (id `id-2`) and `1.0` (id `id-1`). 2.0 changes
    /// `mods/a.jar`, drops `mods/old.jar` and adds `mods/c.jar`.
    pub(crate) fn two_versions() -> Self {
        Self::new(&[
            (
                "id-2",
                "2.0",
                &[
                    ("mods/a.jar", b"a-v2".as_slice()),
                    ("mods/c.jar", b"c".as_slice()),
                    ("config/b.toml", b"b".as_slice()),
                ],
            ),
            (
                "id-1",
                "1.0",
                &[
                    ("mods/a.jar", b"a-v1".as_slice()),
                    ("mods/old.jar", b"old".as_slice()),
                    ("config/b.toml", b"b".as_slice()),
                ],
            ),
        ])
    }

    /// The version [`FakeProvider::single`] offers.
    pub(crate) fn version() -> PackVersion {
        PackVersion {
            id: "v2".to_string(),
            name: "2.0".to_string(),
            ..Default::default()
        }
    }

    fn list(&self) -> Vec<PackVersion> {
        self.versions
            .iter()
            .map(|(id, name, _)| PackVersion {
                id: id.clone(),
                name: name.clone(),
                ..Default::default()
            })
            .collect()
    }

    fn files_of(&self, id: &str) -> Result<&[(String, Vec<u8>)]> {
        self.versions
            .iter()
            .find(|(version, _, _)| version == id)
            .map(|(_, _, files)| files.as_slice())
            .ok_or_else(|| PackError::NotFound(format!("fake version '{id}'")))
    }
}

#[async_trait::async_trait]
impl PackProvider for FakeProvider {
    async fn list_versions(&self, _pack: &PackRef) -> Result<Vec<PackVersion>> {
        Ok(self.list())
    }

    async fn resolve_version(
        &self,
        pack: &PackRef,
        selector: &VersionSelector,
    ) -> Result<ResolvedPackVersion> {
        let versions = self.list();
        let version = resolve_version_from_list(&versions, selector, "fake pack")?;
        Ok(ResolvedPackVersion {
            pack: pack.clone(),
            version: version.clone(),
        })
    }

    async fn prepare(&self, version: &ResolvedPackVersion, staging: &Path) -> Result<PreparedPack> {
        let root = staging.join("server");
        std::fs::create_dir_all(&root).unwrap();
        let mut files = Vec::new();
        for (rel, data) in self.files_of(&version.version.id)? {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, data).unwrap();
            files.push(PreparedFile {
                rel_path: PathBuf::from(rel),
                size: data.len() as u64,
                sha256: sha256_bytes(data),
            });
        }
        Ok(PreparedPack {
            name: "fixture-pack".to_string(),
            version: version.version.clone(),
            root,
            files,
            requirements: None,
            notices: Vec::new(),
        })
    }

    async fn version_fingerprints(
        &self,
        _pack: &PackRef,
        versions: &[PackVersion],
    ) -> Result<Vec<VersionFingerprint>> {
        versions
            .iter()
            .map(|version| {
                let jars = self
                    .files_of(&version.id)?
                    .iter()
                    .filter_map(|(rel, data)| {
                        let name = rel.strip_prefix("mods/")?;
                        (!name.contains('/') && name.ends_with(".jar")).then(|| JarPrint {
                            file_name: name.to_string(),
                            size: data.len() as u64,
                            crc32: Some(crc32fast::hash(data)),
                            sha1: None,
                        })
                    })
                    .collect();
                Ok(VersionFingerprint {
                    version: version.clone(),
                    jars,
                })
            })
            .collect()
    }
}

/// A tiny HTTP/1.1 server on 127.0.0.1 for network-free download and client
/// tests. Each route answers one fixed response; unknown paths get 404.
pub mod http_server {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// One canned response.
    #[derive(Clone)]
    pub struct Route {
        pub path: String,
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Route {
        /// `200 OK` with `body`.
        pub fn ok(path: &str, content_type: &str, body: &[u8]) -> Self {
            Route {
                path: path.to_string(),
                status: 200,
                headers: vec![("Content-Type".to_string(), content_type.to_string())],
                body: body.to_vec(),
            }
        }

        /// `200 OK` with a JSON body.
        pub fn json(path: &str, body: &str) -> Self {
            Self::ok(path, "application/json; charset=utf-8", body.as_bytes())
        }

        /// An empty response with `status`.
        pub fn status(path: &str, status: u16) -> Self {
            Route {
                path: path.to_string(),
                status,
                headers: Vec::new(),
                body: Vec::new(),
            }
        }

        /// Adds a response header.
        pub fn with_header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_string(), value.to_string()));
            self
        }
    }

    /// Starts the server and returns its base URL, `http://127.0.0.1:{port}`.
    pub async fn serve(routes: Vec<Route>) -> String {
        serve_recording(routes).await.0
    }

    /// Like [`serve`], and also returns the raw request heads it received
    /// (request line and headers, lowercased).
    pub async fn serve_recording(routes: Vec<Route>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(routes);
        let seen = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let routes = Arc::clone(&routes);
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    // Drain the request body, so closing the socket never
                    // resets a connection whose response is still unread.
                    let header_end = head.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let content_length = String::from_utf8_lossy(&head[..header_end])
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    let mut body_read = head.len() - header_end;
                    while body_read < content_length {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body_read += n,
                        }
                    }
                    let head = String::from_utf8_lossy(&head[..header_end]).to_lowercase();
                    seen.lock().unwrap().push(head.clone());
                    let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let path = target.split('?').next().unwrap_or("/");
                    let route = routes
                        .iter()
                        .find(|route| route.path.to_lowercase() == path)
                        .cloned()
                        .unwrap_or_else(|| Route::status(path, 404));
                    let mut response = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        route.status,
                        route.body.len()
                    );
                    for (name, value) in &route.headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&route.body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (base, requests)
    }
}
