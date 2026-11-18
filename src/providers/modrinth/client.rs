//! HTTP client for the Modrinth API (v2).
//!
//! Public reads need no key, so this client never sends credentials. Every
//! request carries packctl's `User-Agent`, as Modrinth requires. A 429 is
//! retried once after the `X-Ratelimit-Reset` delay (at most 60 seconds).

use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap};
use reqwest::{Client, ClientBuilder, StatusCode, Url};
use serde::de::DeserializeOwned;

use crate::error::{PackError, Result};
use crate::providers::http::{
    CONNECT_TIMEOUT, REQUEST_TIMEOUT, USER_AGENT, error_chain, https_redirect_policy,
};
use crate::providers::sanitize;

use super::models::{MrProject, MrVersion};
use super::mrpack::DownloadHosts;

/// The public Modrinth API.
pub const DEFAULT_API_BASE: &str = "https://api.modrinth.com/v2";

/// Modrinth's staging API, for packs on staging.modrinth.com.
pub const STAGING_API_BASE: &str = "https://staging-api.modrinth.com/v2";

/// The host of [`STAGING_API_BASE`].
const STAGING_API_HOST: &str = "staging-api.modrinth.com";

/// Overrides the Modrinth API base for every profile.
pub const ENV_API_BASE: &str = "PACKCTL_MODRINTH_API_BASE";

/// Longest wait honoured for a rate-limit reset.
const MAX_RATE_LIMIT_WAIT_SECS: u64 = 60;
/// Most redirects followed on API requests (https only).
const MAX_API_REDIRECTS: usize = 3;
/// Longest response body excerpt quoted in an error.
const MAX_BODY_EXCERPT: usize = 256;

/// HTTP client for the Modrinth API.
pub struct MrClient {
    http: Client,
    base: String,
}

impl MrClient {
    /// A client for `base`, an API base such as [`DEFAULT_API_BASE`]
    /// (validated and normalized, see [`normalize_api_base`]).
    pub fn new(base: &str) -> Result<Self> {
        let base = normalize_api_base(base)?;
        let builder = ClientBuilder::new()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(https_redirect_policy(MAX_API_REDIRECTS, None));
        // Test builds ignore system proxies so local test servers are
        // reached directly.
        #[cfg(test)]
        let builder = builder.no_proxy();
        let http = builder
            .build()
            .expect("reqwest client construction cannot fail with the configured defaults");
        Ok(Self { http, base })
    }

    /// A client for `$PACKCTL_MODRINTH_API_BASE`, or the public API.
    pub fn from_env() -> Result<Self> {
        Self::new(&env_api_base().unwrap_or_else(|| DEFAULT_API_BASE.to_string()))
    }

    /// The normalized API base this client talks to.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The download hosts packs from this API may use: the staging API's
    /// packs may also download from the staging CDN.
    pub fn download_hosts(&self) -> DownloadHosts {
        let staging = Url::parse(&self.base)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .is_some_and(|host| host == STAGING_API_HOST);
        if staging {
            DownloadHosts::WithStagingCdn
        } else {
            DownloadHosts::Public
        }
    }

    /// Fetches a project by id or slug.
    pub async fn get_project(&self, id_or_slug: &str) -> Result<MrProject> {
        let url = self.project_url(id_or_slug)?;
        self.get_json(&url, id_or_slug, "project").await
    }

    /// Lists every version of a project (changelogs left out).
    pub async fn list_versions(&self, project_id: &str) -> Result<Vec<MrVersion>> {
        let url = self.versions_url(project_id)?;
        self.get_json(&url, project_id, "version list").await
    }

    /// Fetches one version by id.
    pub async fn get_version(&self, version_id: &str) -> Result<MrVersion> {
        let url = self.version_url(version_id)?;
        self.get_json(&url, version_id, "version").await
    }

    /// Looks up the versions that contain files with the given SHA-1 hashes
    /// (`POST /version_files`). Returns a map from each known hash to its
    /// version; unknown hashes are left out.
    pub async fn versions_by_sha1(
        &self,
        hashes: &[String],
    ) -> Result<std::collections::HashMap<String, MrVersion>> {
        if hashes.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let url = self.url_with_segments(&["version_files"], None)?;
        let body = serde_json::json!({ "hashes": hashes, "algorithm": "sha1" });
        self.send_json(
            || self.http.post(&url).json(&body),
            &url,
            "version_files",
            "version files",
        )
        .await
    }

    /// `{base}/project/{id}`, with the id percent-encoded as one segment.
    pub(crate) fn project_url(&self, id_or_slug: &str) -> Result<String> {
        self.url_with_segments(&["project", id_or_slug], None)
    }

    /// `{base}/project/{id}/version?include_changelog=false`.
    pub(crate) fn versions_url(&self, project_id: &str) -> Result<String> {
        self.url_with_segments(
            &["project", project_id, "version"],
            Some("include_changelog=false"),
        )
    }

    /// `{base}/version/{id}`.
    pub(crate) fn version_url(&self, version_id: &str) -> Result<String> {
        self.url_with_segments(&["version", version_id], None)
    }

    fn url_with_segments(&self, segments: &[&str], query: Option<&str>) -> Result<String> {
        if segments
            .iter()
            .any(|segment| segment.is_empty() || matches!(*segment, "." | ".."))
        {
            return Err(PackError::Config(format!(
                "'{}' is not a valid Modrinth id or slug",
                sanitize(segments.last().copied().unwrap_or_default())
            )));
        }
        let mut url = Url::parse(&self.base)
            .map_err(|e| PackError::Config(format!("invalid Modrinth API base: {e}")))?;
        url.path_segments_mut()
            .map_err(|()| PackError::Config("invalid Modrinth API base".to_string()))?
            .pop_if_empty()
            .extend(segments);
        url.set_query(query);
        Ok(url.to_string())
    }

    /// GETs `url` and parses its JSON body, retrying once on HTTP 429.
    /// `subject` names what was asked for in a not-found message.
    async fn get_json<T: DeserializeOwned>(
        &self,
        url: &str,
        subject: &str,
        what: &str,
    ) -> Result<T> {
        self.send_json(|| self.http.get(url), url, subject, what)
            .await
    }

    /// Sends the request `make` builds and parses its JSON body, retrying
    /// once on HTTP 429. `subject` names what was asked for in a not-found
    /// message.
    async fn send_json<T: DeserializeOwned>(
        &self,
        make: impl Fn() -> reqwest::RequestBuilder,
        url: &str,
        subject: &str,
        what: &str,
    ) -> Result<T> {
        let mut retried = false;
        loop {
            let response = make().send().await.map_err(|e| {
                PackError::Network(format!("failed to fetch {url}: {}", error_chain(&e)))
            })?;
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let wait = rate_limit_wait(response.headers());
                if retried {
                    return Err(PackError::Network(format!(
                        "Modrinth rate limit reached; retry in {wait}s"
                    )));
                }
                retried = true;
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(status_error(status, url, subject, &self.base, &body));
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let body = response.text().await.map_err(|e| {
                PackError::Network(format!(
                    "failed to read the response from {url}: {}",
                    error_chain(&e)
                ))
            })?;
            if content_type.is_some_and(|ct| !ct.to_ascii_lowercase().contains("json"))
                || body.trim_start().starts_with('<')
            {
                return Err(PackError::Network(format!(
                    "{} returned a non-JSON response for {url}; the Modrinth API may be down",
                    self.base
                )));
            }
            return serde_json::from_str(&body).map_err(|e| {
                PackError::Parse(format!(
                    "failed to parse Modrinth {what} payload from {url}: {e}"
                ))
            });
        }
    }
}

/// Seconds to wait before retrying a rate-limited request: the
/// `X-Ratelimit-Reset` header, capped at 60, or 1 when it is missing.
pub(crate) fn rate_limit_wait(headers: &HeaderMap) -> u64 {
    headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(1)
        .min(MAX_RATE_LIMIT_WAIT_SECS)
}

/// The error for an unsuccessful API status.
fn status_error(status: StatusCode, url: &str, subject: &str, base: &str, body: &str) -> PackError {
    match status {
        StatusCode::NOT_FOUND => PackError::NotFound(format!(
            "Modrinth project or version '{}' not found",
            sanitize(subject)
        )),
        StatusCode::GONE => PackError::Provider(format!(
            "the Modrinth API at {base} has been retired (HTTP 410); update packctl"
        )),
        _ => {
            let excerpt = excerpt(body);
            let detail = if excerpt.is_empty() || body.trim_start().starts_with('<') {
                String::new()
            } else {
                format!(": {excerpt}")
            };
            PackError::Network(format!(
                "request to {url} returned HTTP {}{detail}",
                status.as_u16()
            ))
        }
    }
}

/// A short, single-line, control-free excerpt of a response body.
fn excerpt(body: &str) -> String {
    let cleaned = sanitize(body.trim());
    match cleaned.char_indices().nth(MAX_BODY_EXCERPT) {
        Some((cut, _)) => format!("{}…", &cleaned[..cut]),
        None => cleaned,
    }
}

/// `$PACKCTL_MODRINTH_API_BASE`, when set to a non-blank value.
pub fn env_api_base() -> Option<String> {
    std::env::var(ENV_API_BASE)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Validates and normalizes a Modrinth API base: trims it, strips trailing
/// slashes, and requires `https://` (or `http://` for localhost). Credentials,
/// queries and fragments are rejected.
pub fn normalize_api_base(raw: &str) -> Result<String> {
    let base = raw.trim().trim_end_matches('/');
    let invalid = |reason: &str| {
        PackError::Config(format!(
            "invalid Modrinth API base '{}': {reason}",
            sanitize(raw.trim())
        ))
    };
    let url = Url::parse(base).map_err(|e| invalid(&e.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("credentials in the URL are not supported"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("a query or fragment is not supported"));
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| invalid("the URL has no host"))?;
    match url.scheme() {
        "https" => {}
        "http" if matches!(host, "localhost" | "127.0.0.1" | "[::1]") => {}
        _ => {
            return Err(invalid(
                "use https:// (http:// is allowed only for localhost)",
            ));
        }
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::modrinth::models::fixtures;
    use crate::testutil::http_server::{Route, serve};

    #[test]
    fn url_builders() {
        let client = MrClient::new("https://api.modrinth.com/v2/").unwrap();
        assert_eq!(client.base(), DEFAULT_API_BASE);
        assert_eq!(
            client.project_url("5FFgwNNP").unwrap(),
            "https://api.modrinth.com/v2/project/5FFgwNNP"
        );
        assert_eq!(
            client.versions_url("5FFgwNNP").unwrap(),
            "https://api.modrinth.com/v2/project/5FFgwNNP/version?include_changelog=false"
        );
        assert_eq!(
            client.version_url("YQpPe5Iu").unwrap(),
            "https://api.modrinth.com/v2/version/YQpPe5Iu"
        );
        // Ids are one percent-encoded path segment: no traversal, no query.
        assert_eq!(
            client.project_url("a/b?c#d e").unwrap(),
            "https://api.modrinth.com/v2/project/a%2Fb%3Fc%23d%20e"
        );
        assert!(client.project_url("..").is_err());
        assert!(client.project_url("").is_err());
    }

    #[test]
    fn staging_cdn_only_for_the_staging_api() {
        let hosts = |base: &str| MrClient::new(base).unwrap().download_hosts();
        assert_eq!(hosts(DEFAULT_API_BASE), DownloadHosts::Public);
        assert_eq!(hosts(STAGING_API_BASE), DownloadHosts::WithStagingCdn);
        assert_eq!(
            hosts("https://STAGING-API.modrinth.com/v3/"),
            DownloadHosts::WithStagingCdn
        );
        assert_eq!(
            hosts("https://staging-api.modrinth.com.evil.example/v2"),
            DownloadHosts::Public
        );
    }

    #[test]
    fn api_base_normalization() {
        assert_eq!(
            normalize_api_base(" https://staging-api.modrinth.com/v2/ ").unwrap(),
            STAGING_API_BASE
        );
        assert_eq!(
            normalize_api_base("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        for bad in [
            "http://api.modrinth.com/v2",
            "ftp://x",
            "https://user:pw@api.modrinth.com/v2",
            "https://api.modrinth.com/v2?x=1",
            "not a url",
        ] {
            assert!(
                matches!(normalize_api_base(bad), Err(PackError::Config(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn rate_limit_wait_reads_and_caps_the_reset_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(rate_limit_wait(&headers), 1);
        headers.insert("x-ratelimit-reset", "7".parse().unwrap());
        assert_eq!(rate_limit_wait(&headers), 7);
        headers.insert("x-ratelimit-reset", "3600".parse().unwrap());
        assert_eq!(rate_limit_wait(&headers), 60);
    }

    #[tokio::test]
    async fn fetches_project_and_versions() {
        let base = serve(vec![
            Route::json("/v2/project/cobblemon-modpack", fixtures::PROJECT),
            Route::json("/v2/project/5FFgwNNP/version", fixtures::VERSIONS),
        ])
        .await;
        let client = MrClient::new(&format!("{base}/v2")).unwrap();
        let project = client.get_project("cobblemon-modpack").await.unwrap();
        assert_eq!(project.id, "5FFgwNNP");
        let versions = client.list_versions(&project.id).await.unwrap();
        assert_eq!(versions.len(), 4);
    }

    #[tokio::test]
    async fn status_errors_are_explained() {
        let base = serve(vec![
            Route::status("/v2/project/gone", 410),
            Route::status("/v2/project/busy", 429).with_header("X-Ratelimit-Reset", "0"),
            Route::status("/v2/project/broken", 502),
        ])
        .await;
        let client = MrClient::new(&format!("{base}/v2")).unwrap();

        let err = client.get_project("missing").await.unwrap_err();
        assert!(
            matches!(&err, PackError::NotFound(m) if m == "Modrinth project or version 'missing' not found"),
            "{err:?}"
        );

        let err = client.get_project("gone").await.unwrap_err();
        assert!(
            matches!(&err, PackError::Provider(m) if *m == format!(
                "the Modrinth API at {base}/v2 has been retired (HTTP 410); update packctl"
            )),
            "{err:?}"
        );

        // A second 429 after the retry gives up.
        let err = client.get_project("busy").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("Modrinth rate limit reached; retry in 0s"),
            "{err}"
        );

        let err = client.get_project("broken").await.unwrap_err();
        assert!(err.to_string().contains("returned HTTP 502"), "{err}");
    }
}
