//! Shared, verified HTTP downloads.
//!
//! Every provider downloads through [`download_verified`]: the bytes stream
//! into a uniquely named partial file next to the destination, are size
//! checked as they arrive, and are hashed with every algorithm the caller
//! expects. Only a download whose size and hashes all match is renamed into
//! place; anything else removes the partial file and leaves the destination
//! untouched. The partial file is also removed when a download is cancelled
//! (its future dropped), for example when a parallel download fails first.
//!
//! Redirects follow [`https_redirect_policy`]: https only, bounded, and
//! optionally limited to hosts a caller trusts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder, StatusCode, Url};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;

use crate::error::{PackError, Result};

/// The `User-Agent` every packctl HTTP client sends.
pub const USER_AGENT: &str = concat!(
    "packctl/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/sennecools/packctl)"
);

/// Upper bound on a whole API request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound on establishing a connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a download may sit without receiving data. Downloads have no
/// total timeout, since large server packs on slow links take a while.
pub const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// A hash a download must match, as lowercase or uppercase hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedHash {
    Sha1(String),
    Sha256(String),
    Sha512(String),
}

impl ExpectedHash {
    /// The algorithm's display name, e.g. `SHA-1`.
    pub fn algo(&self) -> &'static str {
        match self {
            ExpectedHash::Sha1(_) => "SHA-1",
            ExpectedHash::Sha256(_) => "SHA-256",
            ExpectedHash::Sha512(_) => "SHA-512",
        }
    }

    /// The expected hex digest.
    pub fn value(&self) -> &str {
        match self {
            ExpectedHash::Sha1(v) | ExpectedHash::Sha256(v) | ExpectedHash::Sha512(v) => v,
        }
    }

    /// Length of a well-formed hex digest for this algorithm.
    fn hex_len(&self) -> usize {
        match self {
            ExpectedHash::Sha1(_) => 40,
            ExpectedHash::Sha256(_) => 64,
            ExpectedHash::Sha512(_) => 128,
        }
    }

    fn is_well_formed(&self) -> bool {
        let value = self.value();
        value.len() == self.hex_len() && value.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

/// What a download must look like to be accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpectedDownload {
    /// Exact size in bytes, when the provider reports one.
    pub size: Option<u64>,
    /// Hashes the bytes must match. Must not be empty.
    pub hashes: Vec<ExpectedHash>,
}

/// Digests computed over a download, for the algorithms that were asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ComputedHashes {
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub sha512: Option<String>,
}

impl ComputedHashes {
    fn get(&self, expected: &ExpectedHash) -> Option<&str> {
        match expected {
            ExpectedHash::Sha1(_) => self.sha1.as_deref(),
            ExpectedHash::Sha256(_) => self.sha256.as_deref(),
            ExpectedHash::Sha512(_) => self.sha512.as_deref(),
        }
    }
}

/// Incremental hashers for exactly the algorithms a download expects.
#[derive(Default)]
pub(crate) struct Hashers {
    sha1: Option<Sha1>,
    sha256: Option<Sha256>,
    sha512: Option<Sha512>,
}

impl Hashers {
    pub(crate) fn for_expected(expected: &ExpectedDownload) -> Self {
        let mut hashers = Hashers::default();
        for hash in &expected.hashes {
            match hash {
                ExpectedHash::Sha1(_) => hashers.sha1 = Some(Sha1::new()),
                ExpectedHash::Sha256(_) => hashers.sha256 = Some(Sha256::new()),
                ExpectedHash::Sha512(_) => hashers.sha512 = Some(Sha512::new()),
            }
        }
        hashers
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        if let Some(h) = &mut self.sha1 {
            h.update(bytes);
        }
        if let Some(h) = &mut self.sha256 {
            h.update(bytes);
        }
        if let Some(h) = &mut self.sha512 {
            h.update(bytes);
        }
    }

    pub(crate) fn finish(self) -> ComputedHashes {
        ComputedHashes {
            sha1: self.sha1.map(|h| format!("{:x}", h.finalize())),
            sha256: self.sha256.map(|h| format!("{:x}", h.finalize())),
            sha512: self.sha512.map(|h| format!("{:x}", h.finalize())),
        }
    }
}

/// A redirect policy that follows at most `max` redirects, only to https
/// URLs, and, when `host_filter` is given, only to hosts it accepts.
pub fn https_redirect_policy(max: usize, host_filter: Option<fn(&str) -> bool>) -> Policy {
    Policy::custom(move |attempt| {
        match redirect_verdict(attempt.url(), attempt.previous().len(), max, host_filter) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    })
}

/// Whether a redirect to `target` may be followed. `previous` counts the
/// URLs already requested in the chain, including the first one.
pub(crate) fn redirect_verdict(
    target: &Url,
    previous: usize,
    max: usize,
    host_filter: Option<fn(&str) -> bool>,
) -> std::result::Result<(), String> {
    if previous > max {
        return Err(format!("too many redirects (more than {max})"));
    }
    if target.scheme() != "https" {
        return Err(format!(
            "refusing to follow a redirect to non-https URL {target}"
        ));
    }
    let host = target.host_str().unwrap_or_default();
    if let Some(allowed) = host_filter
        && !allowed(host)
    {
        return Err(format!("refusing to follow a redirect to host '{host}'"));
    }
    Ok(())
}

/// A client for downloads: packctl's user agent, connect and read timeouts,
/// and the given redirect policy.
pub fn download_client(policy: Policy) -> Client {
    ClientBuilder::new()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .redirect(policy)
        .build()
        .expect("reqwest client construction cannot fail with the configured defaults")
}

/// The display text of a reqwest error followed by its sources, so a refused
/// redirect explains why.
pub(crate) fn error_chain(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

/// Checks that `expected` names at least one well-formed hash.
pub(crate) fn validate_expected(url: &str, expected: &ExpectedDownload) -> Result<()> {
    if expected.hashes.is_empty() {
        return Err(PackError::Provider(format!(
            "refusing to download {url} without a hash to verify"
        )));
    }
    for hash in &expected.hashes {
        if !hash.is_well_formed() {
            return Err(PackError::Provider(format!(
                "{} hash for {url} is malformed",
                hash.algo()
            )));
        }
    }
    Ok(())
}

/// Checks a finished download's size and digests against `expected`.
pub(crate) fn check_hashes(
    url: &str,
    expected: &ExpectedDownload,
    size: u64,
    computed: &ComputedHashes,
) -> Result<()> {
    validate_expected(url, expected)?;
    if let Some(wanted) = expected.size
        && size != wanted
    {
        return Err(PackError::Provider(format!(
            "download from {url} is {size} bytes; expected {wanted}"
        )));
    }
    for hash in &expected.hashes {
        let actual = computed.get(hash).ok_or_else(|| {
            PackError::Other(format!(
                "{} of the download from {url} was not computed",
                hash.algo()
            ))
        })?;
        if !actual.eq_ignore_ascii_case(hash.value()) {
            return Err(PackError::Provider(format!(
                "download from {url} failed {} verification (expected {}, got {actual})",
                hash.algo(),
                hash.value().to_ascii_lowercase()
            )));
        }
    }
    Ok(())
}

/// Downloads `url` to `dest`, verifying size and hashes before the file
/// appears at `dest`. `extra_headers` are sent with this request only.
pub async fn download_verified(
    client: &Client,
    url: &str,
    extra_headers: Option<HeaderMap>,
    dest: &Path,
    expected: &ExpectedDownload,
) -> Result<()> {
    download_verified_with(
        client,
        url,
        extra_headers,
        dest,
        expected,
        &default_status_error,
    )
    .await
}

/// Maps an unsuccessful HTTP status on a download to an error.
pub(crate) type StatusError = dyn Fn(&str, StatusCode) -> PackError + Send + Sync;

fn default_status_error(url: &str, status: StatusCode) -> PackError {
    PackError::Network(format!("download from {url} returned HTTP {status}"))
}

/// [`download_verified`] with a caller-specific message for HTTP errors.
pub(crate) async fn download_verified_with(
    client: &Client,
    url: &str,
    extra_headers: Option<HeaderMap>,
    dest: &Path,
    expected: &ExpectedDownload,
    status_error: &StatusError,
) -> Result<()> {
    validate_expected(url, expected)?;
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| PackError::io(format!("create directory '{}'", parent.display()), e))?;
    }
    let (guard, output) = create_partial(dest)?;
    let partial = guard.path().to_path_buf();

    let result = async {
        let mut output = output;
        let mut request = client.get(url);
        if let Some(headers) = extra_headers {
            request = request.headers(headers);
        }
        let response = request.send().await.map_err(|e| {
            PackError::Network(format!(
                "failed to start download from {url}: {}",
                error_chain(&e)
            ))
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(url, status));
        }
        if let (Some(wanted), Some(announced)) = (expected.size, response.content_length())
            && announced > wanted
        {
            return Err(too_large(url, wanted));
        }

        let mut hashers = Hashers::for_expected(expected);
        let mut size = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                PackError::Network(format!(
                    "download from {url} was interrupted: {}",
                    error_chain(&e)
                ))
            })?;
            size = size.checked_add(chunk.len() as u64).ok_or_else(|| {
                PackError::Provider(format!("download from {url} exceeds supported size"))
            })?;
            if let Some(wanted) = expected.size
                && size > wanted
            {
                return Err(too_large(url, wanted));
            }
            hashers.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .map_err(|e| PackError::io(format!("write '{}'", partial.display()), e))?;
        }
        output
            .flush()
            .await
            .map_err(|e| PackError::io(format!("flush '{}'", partial.display()), e))?;
        output
            .sync_all()
            .await
            .map_err(|e| PackError::io(format!("sync '{}'", partial.display()), e))?;
        drop(output);

        check_hashes(url, expected, size, &hashers.finish())
    }
    .await;

    if let Err(download_error) = result {
        if let Err(cleanup_error) = guard.remove().await {
            return Err(PackError::Provider(format!(
                "{download_error}; also failed to remove partial download '{}': {cleanup_error}",
                partial.display()
            )));
        }
        return Err(download_error);
    }
    guard
        .persist(dest)
        .await
        .map_err(|e| PackError::io(format!("move verified download to '{}'", dest.display()), e))
}

fn too_large(url: &str, wanted: u64) -> PackError {
    PackError::Provider(format!(
        "download from {url} is larger than the expected {wanted} bytes"
    ))
}

/// Distinguishes partial files created by this process.
static PARTIAL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Longest part of the destination's file name kept in a partial name.
const PARTIAL_NAME_KEEP: usize = 96;

/// Creates a fresh, uniquely named partial file next to `dest`:
/// `.{file name}.{pid}.{counter}.packctl-partial`.
///
/// The name extends the full file name, so `a.toml` and `a.json` never share
/// a partial file, and `create_new` guarantees no other download (or pack
/// file) is reused or truncated.
///
/// The file is created synchronously, so there is no point at which a
/// cancelled caller has a file on disk but no [`PartialFile`] guard for it.
pub(crate) fn create_partial(dest: &Path) -> Result<(PartialFile, tokio::fs::File)> {
    let file_name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| {
            PackError::Other(format!(
                "download destination '{}' has no file name",
                dest.display()
            ))
        })?;
    let mut keep = file_name.as_str();
    if keep.len() > PARTIAL_NAME_KEEP {
        let mut end = PARTIAL_NAME_KEEP;
        while !keep.is_char_boundary(end) {
            end -= 1;
        }
        keep = &keep[..end];
    }
    let pid = std::process::id();
    let mut last_error = None;
    for _ in 0..16 {
        let counter = PARTIAL_COUNTER.fetch_add(1, Ordering::Relaxed);
        let partial = dest.with_file_name(format!(".{keep}.{pid}.{counter}.packctl-partial"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
        {
            Ok(file) => {
                let guard = PartialFile {
                    path: partial,
                    armed: true,
                };
                return Ok((guard, tokio::fs::File::from_std(file)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_error = Some(e),
            Err(e) => {
                return Err(PackError::io(format!("create '{}'", partial.display()), e));
            }
        }
    }
    Err(PackError::io(
        format!("create a partial download next to '{}'", dest.display()),
        last_error.unwrap_or_else(|| std::io::Error::other("no free partial file name")),
    ))
}

/// A partial file created by [`create_partial`].
///
/// Dropping the guard removes the file, so a download that fails or whose
/// future is dropped part-way (a parallel download failed first and the rest
/// were cancelled) never leaves one behind. [`PartialFile::persist`] renames
/// it into place instead.
#[derive(Debug)]
pub(crate) struct PartialFile {
    path: PathBuf,
    armed: bool,
}

impl PartialFile {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Renames the partial file to `dest`. When the rename fails, the
    /// partial file is removed as the guard drops.
    pub(crate) async fn persist(mut self, dest: &Path) -> std::io::Result<()> {
        tokio::fs::rename(&self.path, dest).await?;
        self.armed = false;
        Ok(())
    }

    /// Removes the partial file now, reporting a failure to do so.
    pub(crate) async fn remove(mut self) -> Result<()> {
        let result = match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PackError::io(
                format!("remove partial download '{}'", self.path.display()),
                e,
            )),
        };
        if result.is_ok() {
            self.armed = false;
        }
        result
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: drop cannot report an error, and the staging
            // directory holding the file is removed as a whole later anyway.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Downloads one file from a list of mirror URLs. Tests substitute a fake.
#[async_trait::async_trait]
pub trait Fetcher: Send + Sync {
    /// Downloads the first URL that yields verified bytes into `dest`.
    async fn fetch(&self, urls: &[String], dest: &Path, expected: &ExpectedDownload) -> Result<()>;
}

/// The real [`Fetcher`]: [`download_verified`] over each URL in turn.
pub struct HttpFetcher {
    pub client: Client,
}

impl HttpFetcher {
    /// A fetcher whose client follows up to 5 https redirects to any host.
    pub fn new() -> Self {
        Self::with_client(download_client(https_redirect_policy(5, None)))
    }

    /// A fetcher over a caller-built client.
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }
}

impl Default for HttpFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Fetcher for HttpFetcher {
    async fn fetch(&self, urls: &[String], dest: &Path, expected: &ExpectedDownload) -> Result<()> {
        let mut last_error = None;
        for url in urls {
            match download_verified(&self.client, url, None, dest, expected).await {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            PackError::Provider(format!(
                "no download URL for '{}'",
                dest.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::http_server::{Route, serve};

    const ABC_SHA1: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn digests(bytes: &[u8]) -> ComputedHashes {
        let expected = ExpectedDownload {
            size: None,
            hashes: vec![
                ExpectedHash::Sha1(String::new()),
                ExpectedHash::Sha256(String::new()),
                ExpectedHash::Sha512(String::new()),
            ],
        };
        let mut hashers = Hashers::for_expected(&expected);
        hashers.update(bytes);
        hashers.finish()
    }

    fn expect_all(bytes: &[u8]) -> ExpectedDownload {
        let computed = digests(bytes);
        ExpectedDownload {
            size: Some(bytes.len() as u64),
            hashes: vec![
                ExpectedHash::Sha1(computed.sha1.unwrap()),
                ExpectedHash::Sha256(computed.sha256.unwrap()),
                ExpectedHash::Sha512(computed.sha512.unwrap()),
            ],
        }
    }

    fn provider_message(err: PackError) -> String {
        match err {
            PackError::Provider(message) => message,
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn check_hashes_accepts_matching_sha1_sha256_sha512() {
        let computed = digests(b"abc");
        assert_eq!(computed.sha1.as_deref(), Some(ABC_SHA1));
        assert_eq!(computed.sha256.as_deref(), Some(ABC_SHA256));
        let expected = expect_all(b"abc");
        check_hashes("https://x/abc", &expected, 3, &computed).unwrap();

        // Uppercase expected hex is fine too.
        let upper = ExpectedDownload {
            size: Some(3),
            hashes: vec![ExpectedHash::Sha1(ABC_SHA1.to_ascii_uppercase())],
        };
        check_hashes("https://x/abc", &upper, 3, &computed).unwrap();
    }

    #[test]
    fn check_hashes_rejects_mismatch_with_algo_in_message() {
        let computed = digests(b"abd");
        for (index, algo) in ["SHA-1", "SHA-256", "SHA-512"].into_iter().enumerate() {
            let full = expect_all(b"abc");
            let expected = ExpectedDownload {
                size: Some(3),
                hashes: vec![full.hashes[index].clone()],
            };
            let message = provider_message(
                check_hashes("https://x/abc", &expected, 3, &computed).unwrap_err(),
            );
            assert!(
                message.starts_with(&format!(
                    "download from https://x/abc failed {algo} verification (expected "
                )),
                "{message}"
            );
        }
    }

    #[test]
    fn check_hashes_rejects_wrong_size() {
        let computed = digests(b"abc");
        let expected = expect_all(b"abc");
        let message =
            provider_message(check_hashes("https://x/abc", &expected, 4, &computed).unwrap_err());
        assert_eq!(
            message,
            "download from https://x/abc is 4 bytes; expected 3"
        );
    }

    #[test]
    fn check_hashes_rejects_empty_expected() {
        let expected = ExpectedDownload {
            size: Some(3),
            hashes: Vec::new(),
        };
        let message = provider_message(
            check_hashes("https://x/abc", &expected, 3, &digests(b"abc")).unwrap_err(),
        );
        assert_eq!(
            message,
            "refusing to download https://x/abc without a hash to verify"
        );
    }

    #[test]
    fn check_hashes_rejects_malformed_hex() {
        for (hash, algo) in [
            (ExpectedHash::Sha1("abc".to_string()), "SHA-1"),
            (ExpectedHash::Sha1("z".repeat(40)), "SHA-1"),
            (ExpectedHash::Sha256(ABC_SHA1.to_string()), "SHA-256"),
            (ExpectedHash::Sha512(ABC_SHA256.to_string()), "SHA-512"),
        ] {
            let expected = ExpectedDownload {
                size: None,
                hashes: vec![hash],
            };
            let message = provider_message(
                check_hashes("https://x/abc", &expected, 3, &digests(b"abc")).unwrap_err(),
            );
            assert_eq!(
                message,
                format!("{algo} hash for https://x/abc is malformed")
            );
        }
    }

    #[test]
    fn redirect_verdict_table() {
        let url = |s: &str| Url::parse(s).unwrap();
        fn forgecdn(host: &str) -> bool {
            host.ends_with(".forgecdn.net")
        }
        assert!(redirect_verdict(&url("https://a.example/x"), 1, 5, None).is_ok());
        assert!(redirect_verdict(&url("https://a.example/x"), 5, 5, None).is_ok());
        assert!(redirect_verdict(&url("https://a.example/x"), 6, 5, None).is_err());
        assert!(redirect_verdict(&url("http://a.example/x"), 1, 5, None).is_err());
        assert!(
            redirect_verdict(
                &url("https://mediafilez.forgecdn.net/x"),
                1,
                5,
                Some(forgecdn)
            )
            .is_ok()
        );
        assert!(redirect_verdict(&url("https://evil.example/x"), 1, 5, Some(forgecdn)).is_err());
    }

    /// Lists file names in `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn test_client() -> Client {
        ClientBuilder::new()
            .no_proxy()
            .redirect(https_redirect_policy(5, None))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn download_verified_writes_only_verified_bytes() {
        let base = serve(vec![Route::ok("/abc", "application/octet-stream", b"abc")]).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mods").join("abc.jar");
        download_verified(
            &test_client(),
            &format!("{base}/abc"),
            None,
            &dest,
            &expect_all(b"abc"),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        assert_eq!(names_in(&dir.path().join("mods")), vec!["abc.jar"]);
    }

    #[tokio::test]
    async fn partial_file_removed_on_failure() {
        let base = serve(vec![
            Route::ok("/abd", "application/octet-stream", b"abd"),
            Route::ok("/long", "application/octet-stream", b"abcdef"),
            Route::status("/missing", 404),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("pack.zip");
        // A stale file with the old-style partial name must survive untouched.
        std::fs::write(dir.path().join("pack.partial"), b"keep").unwrap();

        // Hash mismatch.
        let err = download_verified(
            &test_client(),
            &format!("{base}/abd"),
            None,
            &dest,
            &expect_all(b"abc"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("failed SHA-1 verification"),
            "{err}"
        );

        // Larger than the expected size.
        let err = download_verified(
            &test_client(),
            &format!("{base}/long"),
            None,
            &dest,
            &expect_all(b"abc"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("is larger than the expected 3 bytes"),
            "{err}"
        );

        // HTTP error.
        let err = download_verified(
            &test_client(),
            &format!("{base}/missing"),
            None,
            &dest,
            &expect_all(b"abc"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("HTTP 404"), "{err}");

        assert!(!dest.exists());
        assert_eq!(names_in(dir.path()), vec!["pack.partial"]);
        assert_eq!(
            std::fs::read(dir.path().join("pack.partial")).unwrap(),
            b"keep"
        );
    }

    #[tokio::test]
    async fn same_stem_downloads_run_concurrently_without_sharing_a_partial() {
        let toml = vec![b't'; 256 * 1024];
        let json = vec![b'j'; 256 * 1024];
        let base = serve(vec![
            Route::ok("/a.toml", "application/octet-stream", &toml),
            Route::ok("/a.json", "application/octet-stream", &json),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let fetcher = HttpFetcher::with_client(test_client());
        let toml_dest = dir.path().join("a.toml");
        let json_dest = dir.path().join("a.json");
        let toml_expected = expect_all(&toml);
        let json_expected = expect_all(&json);
        let toml_urls = vec![format!("{base}/a.toml")];
        let json_urls = vec![format!("{base}/a.json")];
        let (first, second) = tokio::join!(
            fetcher.fetch(&toml_urls, &toml_dest, &toml_expected),
            fetcher.fetch(&json_urls, &json_dest, &json_expected),
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(std::fs::read(&toml_dest).unwrap(), toml);
        assert_eq!(std::fs::read(&json_dest).unwrap(), json);
        assert_eq!(names_in(dir.path()), vec!["a.json", "a.toml"]);
    }

    #[tokio::test]
    async fn cancelled_download_removes_its_partial_file() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A server that sends part of the body and then stalls.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\npartial bytes")
                .await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("pack.zip");
        let expected = ExpectedDownload {
            size: Some(1000),
            hashes: vec![ExpectedHash::Sha1("0".repeat(40))],
        };
        let client = test_client();
        let url = format!("{base}/pack.zip");
        let mut download = Box::pin(download_verified(&client, &url, None, &dest, &expected));

        // Drive the download until its partial file exists, then drop it.
        let has_partial = |dir: &Path| {
            names_in(dir)
                .iter()
                .any(|name| name.ends_with(".packctl-partial"))
        };
        for _ in 0..500 {
            tokio::select! {
                result = &mut download => panic!("download finished: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            if has_partial(dir.path()) {
                break;
            }
        }
        assert!(has_partial(dir.path()), "the download never started");
        drop(download);

        // Windows may list a removed file until its last handle closes.
        for _ in 0..100 {
            if names_in(dir.path()).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(names_in(dir.path()), Vec::<String>::new());
    }

    #[tokio::test]
    async fn http_fetcher_falls_back_to_the_next_url() {
        let base = serve(vec![
            Route::status("/dead", 500),
            Route::ok("/abc", "application/octet-stream", b"abc"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("abc.jar");
        let fetcher = HttpFetcher::with_client(test_client());
        fetcher
            .fetch(
                &[format!("{base}/dead"), format!("{base}/abc")],
                &dest,
                &expect_all(b"abc"),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");

        // All URLs failing returns the last error.
        let err = fetcher
            .fetch(&[format!("{base}/dead")], &dest, &expect_all(b"xyz"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 500"), "{err}");
    }

    #[test]
    fn user_agent_names_packctl_and_its_version() {
        assert!(USER_AGENT.starts_with(&format!("packctl/{} ", env!("CARGO_PKG_VERSION"))));
    }
}
