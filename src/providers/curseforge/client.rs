//! HTTP client for the CurseForge API (v1).
//!
//! An API key is optional. The endpoint comes from [`CfEndpoint`]: the
//! official API when the user has a key, otherwise the community proxy
//! `https://api.curse.tools` (see [`super::endpoint`]). The key lives only
//! inside this client and is sent as the `x-api-key` header to the official
//! API (or to a custom endpoint the user explicitly allowed). It is never
//! sent to the community proxy or any other host, never follows a redirect
//! to another host, and is never logged.
//!
//! Downloads come from CurseForge's CDN (`*.forgecdn.net`) in every endpoint
//! mode, over https, and are size and SHA-1 verified. In proxy mode the file
//! metadata, including the SHA-1, is reported by the third-party proxy, so
//! the proxy decides which CDN file is installed.

use std::path::Path;
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder, StatusCode, Url};
use serde::de::DeserializeOwned;

use crate::config::profile::ServerProfile;
use crate::error::{PackError, Result};
use crate::providers::http::{
    self, CONNECT_TIMEOUT, ExpectedDownload, ExpectedHash, REQUEST_TIMEOUT, USER_AGENT,
    error_chain, https_redirect_policy,
};

use super::endpoint::{CfEndpoint, CfEndpointMode};
use super::models::{
    CLASS_ID_MODPACKS, CfFile, CfFileListResponse, CfFileResponse, CfMod, CfModResponse,
    CfModSearchResponse,
};

const API_KEY_HEADER: &str = "x-api-key";
const PAGE_SIZE: u64 = 50;
const MAX_PAGES: u32 = 200;
/// Most redirects followed on API requests (same host, https only).
const MAX_API_REDIRECTS: usize = 3;
/// Most redirects followed on downloads (forgecdn.net hosts, https only).
const MAX_DOWNLOAD_REDIRECTS: usize = 5;
/// Timeout of the `doctor` endpoint probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Game id for Minecraft.
const GAME_ID_MINECRAFT: u32 = 432;
/// Longest response body excerpt quoted in an error.
const MAX_BODY_EXCERPT: usize = 256;

/// HTTP client for the CurseForge API.
pub struct CfClient {
    api_http: Client,
    download_http: Client,
    endpoint: CfEndpoint,
}

impl CfClient {
    /// Builds a client for `endpoint`.
    pub fn new(endpoint: CfEndpoint) -> Self {
        let api_http = client_builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .default_headers(api_headers(&endpoint))
            .redirect(api_redirect_policy(endpoint.base_url.clone()))
            .build()
            .expect("reqwest client construction cannot fail with the configured defaults");
        // Downloads carry no default credentials; the CDN key is added per
        // request, and redirects may only go to forgecdn.net over https.
        let download_http = http::download_client(https_redirect_policy(
            MAX_DOWNLOAD_REDIRECTS,
            Some(key_allowed_for_host),
        ));
        Self {
            api_http,
            download_http,
            endpoint,
        }
    }

    /// Builds the client a profile's endpoint settings and key describe.
    pub fn for_profile(profile: &ServerProfile) -> Result<Self> {
        Ok(Self::new(profile.curseforge_endpoint()?))
    }

    /// A client for `base` with an optional key (sent only when `base` is
    /// the official API).
    #[cfg(test)]
    pub(crate) fn with_base(base: &str, key: Option<&str>) -> Self {
        let endpoint =
            super::endpoint::resolve_endpoint(Some(base), None, key.map(str::to_string), false)
                .expect("test base URL is valid");
        Self::new(endpoint)
    }

    /// The endpoint this client talks to.
    pub fn endpoint(&self) -> &CfEndpoint {
        &self.endpoint
    }

    /// The default headers of API requests.
    #[cfg(test)]
    fn api_default_headers(&self) -> HeaderMap {
        api_headers(&self.endpoint)
    }

    /// Fetches the project with `project_id`.
    pub async fn get_mod(&self, project_id: u32) -> Result<CfMod> {
        let url = self.mod_url(project_id);
        let response = self.send(self.api_http.get(&url), &url).await?;
        let payload: CfModResponse = self.read_json(response, &url, "mod").await?;
        Ok(payload.data)
    }

    /// Checks that the endpoint answers a project lookup within 10 seconds.
    pub async fn probe(&self, project_id: u32) -> Result<()> {
        let url = self.mod_url(project_id);
        let response = self
            .send(self.api_http.get(&url).timeout(PROBE_TIMEOUT), &url)
            .await?;
        let _: CfModResponse = self.read_json(response, &url, "mod").await?;
        Ok(())
    }

    /// Fetches every file of the project with `project_id`, paging through
    /// all pages.
    pub async fn get_files(&self, project_id: u32) -> Result<Vec<CfFile>> {
        let mut files = Vec::new();
        let mut index: u64 = 0;
        for _page in 0..MAX_PAGES {
            let url = self.files_page_url(project_id, index);
            let response = self.send(self.api_http.get(&url), &url).await?;
            let page: CfFileListResponse = self.read_json(response, &url, "file list").await?;
            let total = page.pagination.total_count;
            let data_len = page.data.len();
            files.extend(page.data);
            index += PAGE_SIZE;
            if index >= total || data_len == 0 {
                break;
            }
        }
        Ok(files)
    }

    /// Fetches a single file belonging to the project with `project_id`.
    pub async fn get_file(&self, project_id: u32, file_id: u32) -> Result<CfFile> {
        let url = self.file_url(project_id, file_id);
        let response = self.send(self.api_http.get(&url), &url).await?;
        let payload: CfFileResponse = self.read_json(response, &url, "file").await?;
        Ok(payload.data)
    }

    /// Fetches the Minecraft modpack whose slug is `slug`.
    ///
    /// Slugs are unique only within a project class, so the search is
    /// limited to modpacks (`classId=4471`).
    pub async fn search_by_slug(&self, slug: &str) -> Result<CfMod> {
        let url = self.search_endpoint();
        let request = self.api_http.get(&url).query(&[
            ("gameId", GAME_ID_MINECRAFT.to_string()),
            ("classId", CLASS_ID_MODPACKS.to_string()),
            ("slug", slug.to_string()),
        ]);
        let response = self.send(request, &url).await?;
        let payload: CfModSearchResponse = self.read_json(response, &url, "search").await?;
        pick_modpack(payload.data, slug)
    }

    /// Downloads a CurseForge file and verifies its reported size and SHA-1.
    ///
    /// The download URL must be https on `*.forgecdn.net`, whatever the
    /// endpoint, so a third-party endpoint can only pick real CurseForge
    /// files. `slug` names the project in the message shown when the author
    /// disabled third-party downloads.
    pub async fn download_file_to(&self, file: &CfFile, slug: &str, dest: &Path) -> Result<()> {
        let url = self.download_url_for(file, slug)?;
        let sha1 = file.sha1_hash().ok_or_else(|| {
            PackError::Provider(format!(
                "CurseForge file {} reports no SHA-1 hash; refusing to install an unverified download",
                file.id
            ))
        })?;
        let expected = ExpectedDownload {
            size: (file.file_length > 0).then_some(file.file_length),
            hashes: vec![ExpectedHash::Sha1(sha1.trim().to_string())],
        };
        // The CDN accepts the key; send it only when talking to the official
        // API (the download URL is already known to be https forgecdn.net).
        let headers = match (self.endpoint.mode, self.endpoint.api_key()) {
            (CfEndpointMode::Official, Some(key)) => {
                let mut headers = HeaderMap::new();
                if let Ok(mut value) = HeaderValue::from_str(key) {
                    value.set_sensitive(true);
                    headers.insert(API_KEY_HEADER, value);
                }
                Some(headers)
            }
            _ => None,
        };
        let key_sent = headers.is_some();
        let status_error =
            move |url: &str, status: StatusCode| download_status_error(url, status, key_sent);
        http::download_verified_with(
            &self.download_http,
            url.as_str(),
            headers,
            dest,
            &expected,
            &status_error,
        )
        .await
    }

    /// The CDN URL to download `file` from, after the host policy.
    pub(crate) fn download_url_for(&self, file: &CfFile, slug: &str) -> Result<Url> {
        let raw = file
            .download_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| distribution_disabled(file, slug))?;
        let base = &self.endpoint.base_url;
        let url = Url::parse(raw).map_err(|_| {
            PackError::Provider(format!(
                "CurseForge endpoint {base} returned an invalid download URL for file {}; refusing",
                file.id
            ))
        })?;
        let host = url.host_str().unwrap_or_default();
        if url.scheme() != "https" || !key_allowed_for_host(host) {
            let shown = if url.scheme() == "https" {
                host.to_string()
            } else {
                format!("{}://{host}", url.scheme())
            };
            return Err(PackError::Provider(format!(
                "CurseForge endpoint {base} returned a download URL outside forgecdn.net ({}); refusing",
                sanitize(&shown)
            )));
        }
        Ok(url)
    }

    fn mod_url(&self, project_id: u32) -> String {
        format!("{}/v1/mods/{}", self.endpoint.base_url, project_id)
    }

    /// URL of the file list endpoint for `project_id`.
    pub(crate) fn files_url(&self, project_id: u32) -> String {
        format!("{}/v1/mods/{}/files", self.endpoint.base_url, project_id)
    }

    fn file_url(&self, project_id: u32, file_id: u32) -> String {
        format!(
            "{}/v1/mods/{}/files/{}",
            self.endpoint.base_url, project_id, file_id
        )
    }

    /// URL of one page of the file list, starting at `index`.
    pub(crate) fn files_page_url(&self, project_id: u32, index: u64) -> String {
        format!(
            "{}?pageSize={}&index={}",
            self.files_url(project_id),
            PAGE_SIZE,
            index
        )
    }

    /// Base URL of the mod search endpoint.
    pub(crate) fn search_endpoint(&self) -> String {
        format!("{}/v1/mods/search", self.endpoint.base_url)
    }

    /// Sends an API request and checks its status.
    async fn send(&self, request: reqwest::RequestBuilder, url: &str) -> Result<reqwest::Response> {
        let response = request.send().await.map_err(|e| {
            PackError::Network(format!("failed to fetch {url}: {}", error_chain(&e)))
        })?;
        self.ensure_success(response, url).await
    }

    /// Reads a JSON body, explaining HTML error pages (a proxy that is down
    /// often answers with one) instead of reporting a parse error.
    async fn read_json<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
        url: &str,
        what: &str,
    ) -> Result<T> {
        let status = response.status();
        let content_type = content_type(&response);
        let body = response.text().await.map_err(|e| {
            PackError::Network(format!(
                "failed to read the response from {url}: {}",
                error_chain(&e)
            ))
        })?;
        if looks_like_html(content_type.as_deref(), &body) {
            return Err(PackError::Network(format!(
                "{} returned a non-JSON response (HTTP {}) for {url}; the CurseForge endpoint may \
                 be down. Configure your own key with 'packctl apikey --global' or another \
                 endpoint via PACKCTL_CF_API_BASE",
                self.endpoint.base_url,
                status.as_u16()
            )));
        }
        serde_json::from_str(&body).map_err(|e| {
            PackError::Parse(format!("failed to parse {what} payload from {url}: {e}"))
        })
    }

    async fn ensure_success(
        &self,
        response: reqwest::Response,
        url: &str,
    ) -> Result<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let content_type = content_type(&response);
        let body = response.text().await.unwrap_or_default();
        let reason = status_reason(
            status,
            self.endpoint.mode,
            self.endpoint.api_key().is_some(),
            &self.endpoint.base_url,
        );
        let excerpt = if looks_like_html(content_type.as_deref(), &body) {
            String::new()
        } else {
            excerpt(&body)
        };
        let detail = if excerpt.is_empty() {
            String::new()
        } else {
            format!(": {excerpt}")
        };
        Err(PackError::Network(format!(
            "request to {url} returned HTTP {} ({reason}){detail}",
            status.as_u16()
        )))
    }
}

/// A client builder with packctl's user agent. Test builds ignore system
/// proxies so local test servers are reached directly.
fn client_builder() -> ClientBuilder {
    let builder = ClientBuilder::new().user_agent(USER_AGENT);
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder
}

/// Default API request headers: the key only when the endpoint may get it.
fn api_headers(endpoint: &CfEndpoint) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(key) = endpoint.api_key()
        && let Ok(mut value) = HeaderValue::from_str(key)
    {
        value.set_sensitive(true);
        headers.insert(API_KEY_HEADER, value);
    }
    headers
}

/// API requests follow at most [`MAX_API_REDIRECTS`] redirects, https only,
/// and only to the endpoint's own host, so the key never leaves it.
fn api_redirect_policy(base_url: String) -> Policy {
    Policy::custom(move |attempt| {
        match api_redirect_verdict(&base_url, attempt.url(), attempt.previous().len()) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    })
}

/// Whether an API redirect to `target` may be followed.
pub(crate) fn api_redirect_verdict(
    base_url: &str,
    target: &Url,
    previous: usize,
) -> std::result::Result<(), String> {
    let refuse = || format!("{base_url} redirected to {target}; refusing to follow");
    let base = Url::parse(base_url).map_err(|_| refuse())?;
    let same_origin_host = target.host_str() == base.host_str()
        && target.port_or_known_default() == base.port_or_known_default();
    if previous > MAX_API_REDIRECTS || target.scheme() != "https" || !same_origin_host {
        return Err(refuse());
    }
    Ok(())
}

/// Whether the CurseForge CDN key may be sent to `host`, and whether
/// downloads may come from it: `edge.forgecdn.net` or any `*.forgecdn.net`.
pub(crate) fn key_allowed_for_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "edge.forgecdn.net" || host.ends_with(".forgecdn.net")
}

/// True when a response is an HTML page rather than JSON: the content type
/// does not mention JSON, or the body starts with `<`.
pub(crate) fn looks_like_html(content_type: Option<&str>, body: &str) -> bool {
    content_type.is_some_and(|ct| !ct.to_ascii_lowercase().contains("json"))
        || body.trim_start().starts_with('<')
}

/// The explanation shown for an unsuccessful API status.
pub(crate) fn status_reason(
    status: StatusCode,
    mode: CfEndpointMode,
    has_key: bool,
    base: &str,
) -> String {
    let refused = matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error();
    match mode {
        _ if status == StatusCode::NOT_FOUND => "resource not found".to_string(),
        CfEndpointMode::Official
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) =>
        {
            if has_key {
                "CurseForge rejected the API key; check CF_API_KEY or run 'packctl apikey'"
                    .to_string()
            } else {
                "the official CurseForge API requires a key; store one with 'packctl apikey \
                 --global' (free at https://console.curseforge.com/) or unset \
                 PACKCTL_CF_API_BASE to use the community proxy"
                    .to_string()
            }
        }
        CfEndpointMode::Official if status == StatusCode::TOO_MANY_REQUESTS => {
            "rate limited; retry later".to_string()
        }
        CfEndpointMode::Proxy | CfEndpointMode::Custom if refused => format!(
            "the CurseForge endpoint {base} refused or rate-limited the request; store your own \
             key with 'packctl apikey --global' or set PACKCTL_CF_API_BASE"
        ),
        _ => "unexpected status".to_string(),
    }
}

/// The error for an unsuccessful download status.
fn download_status_error(url: &str, status: StatusCode, key_sent: bool) -> PackError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if key_sent => {
            PackError::Network(format!(
                "CurseForge rejected the API key for the download from {url} (HTTP {}); check \
                 CF_API_KEY or run 'packctl apikey'",
                status.as_u16()
            ))
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => PackError::Network(
            "CurseForge requires an API key for file downloads; store one with 'packctl apikey \
             --global' (free at https://console.curseforge.com/)"
                .to_string(),
        ),
        StatusCode::NOT_FOUND => PackError::Network(format!(
            "download from {url} returned HTTP 404 (resource not found)"
        )),
        _ => PackError::Network(format!(
            "download from {url} returned HTTP {}",
            status.as_u16()
        )),
    }
}

/// The error for a file without a download URL: its author disabled
/// third-party distribution. packctl never works around that.
fn distribution_disabled(file: &CfFile, slug: &str) -> PackError {
    let page = if slug.trim().is_empty() {
        "the pack's CurseForge files page".to_string()
    } else {
        format!(
            "https://www.curseforge.com/minecraft/modpacks/{}/files/{}",
            sanitize(slug.trim()),
            file.id
        )
    };
    PackError::Provider(format!(
        "CurseForge file {} ({}) cannot be downloaded by third-party tools because its author \
         disabled it; download it from {page} and use the local provider (drop it in packs/)",
        file.id,
        sanitize(&file.file_name)
    ))
}

/// Picks the modpack among slug search results. Results of other classes
/// (a mod with the same slug) are never chosen.
pub(crate) fn pick_modpack(results: Vec<CfMod>, slug: &str) -> Result<CfMod> {
    let mut other = None;
    for project in results {
        match project.class_id {
            Some(CLASS_ID_MODPACKS) | None => return Ok(project),
            Some(class) => {
                other.get_or_insert((project.id, class));
            }
        }
    }
    match other {
        Some((id, class)) => Err(PackError::Config(format!(
            "CurseForge project {id} is not a modpack (class {class})"
        ))),
        None => Err(PackError::NotFound(format!(
            "no CurseForge modpack found for slug '{}'",
            sanitize(slug)
        ))),
    }
}

fn content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// A short, single-line, control-free excerpt of a response body.
fn excerpt(body: &str) -> String {
    let cleaned = sanitize(body.trim());
    match cleaned.char_indices().nth(MAX_BODY_EXCERPT) {
        Some((cut, _)) => format!("{}…", &cleaned[..cut]),
        None => cleaned,
    }
}

/// Replaces control characters (newlines, escapes) in endpoint-supplied
/// text with spaces before it reaches the terminal.
pub(crate) use crate::providers::sanitize;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::curseforge::endpoint::{
        DEFAULT_PROXY_BASE_URL, OFFICIAL_BASE_URL, resolve_endpoint,
    };
    use crate::providers::curseforge::models::fixtures;
    use crate::testutil::http_server::{Route, serve, serve_recording};

    fn header_key(client: &CfClient) -> Option<String> {
        client
            .api_default_headers()
            .get(API_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    #[test]
    fn key_header_present_only_for_official() {
        let official = CfClient::with_base(OFFICIAL_BASE_URL, Some("  secret-key  "));
        assert_eq!(header_key(&official).as_deref(), Some("secret-key"));

        let official_without_key = CfClient::with_base(OFFICIAL_BASE_URL, None);
        assert_eq!(header_key(&official_without_key), None);

        let proxy = CfClient::with_base(DEFAULT_PROXY_BASE_URL, Some("secret-key"));
        assert_eq!(header_key(&proxy), None);

        let custom = CfClient::with_base("https://cf.example", Some("secret-key"));
        assert_eq!(header_key(&custom), None);

        // A custom endpoint gets the key only when the user opted in.
        let opted_in = CfClient::new(
            resolve_endpoint(
                Some("https://cf.example"),
                None,
                Some("secret-key".to_string()),
                true,
            )
            .unwrap(),
        );
        assert_eq!(header_key(&opted_in).as_deref(), Some("secret-key"));
    }

    #[test]
    fn url_builders_use_configured_base() {
        for base in [
            OFFICIAL_BASE_URL,
            DEFAULT_PROXY_BASE_URL,
            "https://cf.example/proxy",
        ] {
            let client = CfClient::with_base(base, None);
            assert_eq!(client.mod_url(925200), format!("{base}/v1/mods/925200"));
            assert_eq!(
                client.files_url(925200),
                format!("{base}/v1/mods/925200/files")
            );
            assert_eq!(
                client.file_url(925200, 42),
                format!("{base}/v1/mods/925200/files/42")
            );
            assert_eq!(
                client.files_page_url(925200, 50),
                format!("{base}/v1/mods/925200/files?pageSize=50&index=50")
            );
            assert_eq!(client.search_endpoint(), format!("{base}/v1/mods/search"));
        }
    }

    #[test]
    fn looks_like_html_detects_cloudflare_page_and_json_ok() {
        let cloudflare = "<!DOCTYPE html>\n<html><head><title>Just a moment...</title>";
        assert!(looks_like_html(
            Some("text/html; charset=UTF-8"),
            cloudflare
        ));
        assert!(looks_like_html(None, cloudflare));
        assert!(looks_like_html(Some("application/json"), "  <html>"));
        assert!(looks_like_html(Some("text/plain"), "error code: 1015"));
        assert!(!looks_like_html(
            Some("application/json; charset=utf-8"),
            r#"{"data":{}}"#
        ));
        assert!(!looks_like_html(None, r#"{"data":{}}"#));
    }

    #[test]
    fn status_reason_mode_aware_messages() {
        use CfEndpointMode::{Custom, Official, Proxy};
        let reason = |status: u16, mode, key| {
            status_reason(
                StatusCode::from_u16(status).unwrap(),
                mode,
                key,
                "https://cf.example",
            )
        };
        assert_eq!(
            reason(401, Official, true),
            "CurseForge rejected the API key; check CF_API_KEY or run 'packctl apikey'"
        );
        assert_eq!(reason(403, Official, true), reason(401, Official, true));
        assert_eq!(
            reason(403, Official, false),
            "the official CurseForge API requires a key; store one with 'packctl apikey \
             --global' (free at https://console.curseforge.com/) or unset PACKCTL_CF_API_BASE \
             to use the community proxy"
        );
        let refused = "the CurseForge endpoint https://cf.example refused or rate-limited the \
                       request; store your own key with 'packctl apikey --global' or set \
                       PACKCTL_CF_API_BASE";
        for status in [401, 403, 429, 500, 503] {
            assert_eq!(reason(status, Proxy, false), refused, "status {status}");
            assert_eq!(reason(status, Custom, true), refused, "status {status}");
        }
        for mode in [Official, Proxy, Custom] {
            assert_eq!(reason(404, mode, false), "resource not found");
        }
        assert_eq!(reason(429, Official, true), "rate limited; retry later");
        assert_eq!(reason(500, Official, true), "unexpected status");
        assert_eq!(reason(400, Proxy, false), "unexpected status");
    }

    #[test]
    fn key_allowed_for_host_table() {
        for (host, allowed) in [
            ("edge.forgecdn.net", true),
            ("mediafilez.forgecdn.net", true),
            ("EDGE.FORGECDN.NET", true),
            ("forgecdn.net", false),
            ("evil.com", false),
            ("forgecdn.net.evil.com", false),
            ("evilforgecdn.net", false),
            ("", false),
        ] {
            assert_eq!(key_allowed_for_host(host), allowed, "host: {host}");
        }
    }

    #[test]
    fn api_redirect_verdict_allows_only_same_host_https() {
        let url = |s: &str| Url::parse(s).unwrap();
        let base = "https://api.curseforge.com";
        assert!(
            api_redirect_verdict(base, &url("https://api.curseforge.com/v1/mods/1"), 1).is_ok()
        );
        assert!(api_redirect_verdict(base, &url("https://api.curseforge.com:443/x"), 3).is_ok());
        assert!(api_redirect_verdict(base, &url("https://api.curseforge.com/x"), 4).is_err());
        assert!(api_redirect_verdict(base, &url("https://evil.example/x"), 1).is_err());
        assert!(api_redirect_verdict(base, &url("https://api.curseforge.com:8443/x"), 1).is_err());
        // The legacy proxy layout downgrades to http; never follow that.
        let err = api_redirect_verdict(
            "https://api.curse.tools",
            &url("http://api.curse.tools/v1/mods/925200"),
            1,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "https://api.curse.tools redirected to http://api.curse.tools/v1/mods/925200; refusing to follow"
        );
    }

    fn file_with_url(url: Option<&str>) -> CfFile {
        CfFile {
            id: 6900002,
            file_name: "ServerFiles-4.12.zip".to_string(),
            file_length: 3,
            download_url: url.map(str::to_string),
            ..CfFile::default()
        }
    }

    #[test]
    fn download_url_must_be_https_forgecdn_in_every_mode() {
        for base in [
            OFFICIAL_BASE_URL,
            DEFAULT_PROXY_BASE_URL,
            "https://cf.example",
        ] {
            let client = CfClient::with_base(base, Some("k"));
            let ok = client
                .download_url_for(
                    &file_with_url(Some(
                        "https://edge.forgecdn.net/files/6900/2/ServerFiles-4.12.zip",
                    )),
                    "atm10",
                )
                .unwrap();
            assert_eq!(ok.host_str(), Some("edge.forgecdn.net"));

            // A proxy-supplied URL on another host is rejected.
            let err = client
                .download_url_for(
                    &file_with_url(Some("https://evil.example/ServerFiles-4.12.zip")),
                    "atm10",
                )
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "provider error: CurseForge endpoint {base} returned a download URL outside \
                     forgecdn.net (evil.example); refusing"
                )
            );

            let err = client
                .download_url_for(
                    &file_with_url(Some("http://edge.forgecdn.net/files/1/2/x.zip")),
                    "atm10",
                )
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("(http://edge.forgecdn.net); refusing"),
                "{err}"
            );
        }
    }

    #[test]
    fn missing_download_url_means_distribution_disabled() {
        let client = CfClient::with_base(DEFAULT_PROXY_BASE_URL, None);
        let err = client
            .download_url_for(&file_with_url(None), "all-the-mods-10")
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "provider error: CurseForge file 6900002 (ServerFiles-4.12.zip) cannot be downloaded \
             by third-party tools because its author disabled it; download it from \
             https://www.curseforge.com/minecraft/modpacks/all-the-mods-10/files/6900002 and use \
             the local provider (drop it in packs/)"
        );
    }

    #[tokio::test]
    async fn download_refuses_a_file_without_sha1() {
        let client = CfClient::with_base(DEFAULT_PROXY_BASE_URL, None);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("pack.zip");
        let file = file_with_url(Some("https://edge.forgecdn.net/files/6900/2/x.zip"));
        let err = client
            .download_file_to(&file, "atm10", &dest)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "provider error: CurseForge file 6900002 reports no SHA-1 hash; refusing to install \
             an unverified download"
        );
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn download_status_errors_explain_keys() {
        let unauthorized = download_status_error(
            "https://edge.forgecdn.net/x",
            StatusCode::UNAUTHORIZED,
            false,
        );
        assert_eq!(
            unauthorized.to_string(),
            "network error: CurseForge requires an API key for file downloads; store one with \
             'packctl apikey --global' (free at https://console.curseforge.com/)"
        );
        let rejected =
            download_status_error("https://edge.forgecdn.net/x", StatusCode::FORBIDDEN, true);
        assert!(
            rejected.to_string().contains("rejected the API key"),
            "{rejected}"
        );
    }

    #[test]
    fn pick_modpack_skips_other_classes() {
        let parsed: CfModSearchResponse = serde_json::from_str(fixtures::SEARCH).unwrap();
        let picked = pick_modpack(parsed.data, "create").unwrap();
        assert_eq!(picked.id, 372026);

        let only_mod: CfModSearchResponse = serde_json::from_str(fixtures::SEARCH).unwrap();
        let mods: Vec<CfMod> = only_mod
            .data
            .into_iter()
            .filter(|m| m.class_id == Some(6))
            .collect();
        let err = pick_modpack(mods, "create").unwrap_err();
        assert_eq!(
            err.to_string(),
            "CurseForge project 328085 is not a modpack (class 6)"
        );

        assert!(matches!(
            pick_modpack(Vec::new(), "nope"),
            Err(PackError::NotFound(_))
        ));
    }

    #[test]
    fn excerpt_is_single_line_and_bounded() {
        let long = format!("line one\nline two\u{1b}[31m{}", "é".repeat(400));
        let short = excerpt(&long);
        assert!(!short.contains('\n') && !short.contains('\u{1b}'));
        assert_eq!(short.chars().count(), MAX_BODY_EXCERPT + 1);
    }

    #[tokio::test]
    async fn get_mod_and_get_file_unwrap_the_data_envelope() {
        let base = serve(vec![
            Route::json("/v1/mods/925200", fixtures::MOD_925200),
            Route::json("/v1/mods/925200/files/6900002", fixtures::FILE_SERVER),
        ])
        .await;
        let client = CfClient::with_base(&base, None);
        let project = client.get_mod(925200).await.unwrap();
        assert_eq!(project.slug, "all-the-mods-10");
        let file = client.get_file(925200, 6900002).await.unwrap();
        assert!(file.is_server_pack);
        client.probe(925200).await.unwrap();
    }

    #[tokio::test]
    async fn html_error_page_is_explained_not_parsed() {
        let base = serve(vec![
            Route::ok(
                "/v1/mods/1",
                "text/html; charset=UTF-8",
                b"<!DOCTYPE html><title>Just a moment...</title>",
            ),
            Route::ok("/v1/mods/2", "text/html", b"<html>Bad gateway</html>")
                .with_header("X-Status", "502"),
        ])
        .await;
        let client = CfClient::with_base(&base, None);
        let err = client.get_mod(1).await.unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with(&format!("network error: {base} returned a non-JSON response (HTTP 200) for {base}/v1/mods/1; the CurseForge endpoint may be down.")),
            "{message}"
        );
        assert!(message.contains("PACKCTL_CF_API_BASE"), "{message}");
    }

    #[tokio::test]
    async fn search_asks_for_modpacks_and_custom_endpoints_get_no_key() {
        let (base, requests) =
            serve_recording(vec![Route::json("/v1/mods/search", fixtures::SEARCH)]).await;
        let client = CfClient::with_base(&base, Some("secret-key"));
        let picked = client.search_by_slug("create").await.unwrap();
        assert_eq!(picked.id, 372026);
        let seen = requests.lock().unwrap().join("\n");
        assert!(seen.contains("classid=4471"), "{seen}");
        assert!(seen.contains("gameid=432"), "{seen}");
        assert!(!seen.contains("x-api-key"), "{seen}");
        assert!(!seen.contains("secret-key"), "{seen}");
        assert!(seen.contains("user-agent: packctl/"), "{seen}");
    }

    #[tokio::test]
    async fn error_statuses_are_reported_with_the_endpoint() {
        let base = serve(vec![Route::status("/v1/mods/7", 503)]).await;
        let client = CfClient::with_base(&base, None);
        let err = client.probe(7).await.unwrap_err();
        assert!(
            err.to_string().contains(&format!(
                "returned HTTP 503 (the CurseForge endpoint {base} refused or rate-limited"
            )),
            "{err}"
        );
        let err = client.get_mod(8).await.unwrap_err();
        assert!(
            err.to_string().contains("HTTP 404 (resource not found)"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn get_files_pages_through_results() {
        let base = serve(vec![Route::json(
            "/v1/mods/925200/files",
            fixtures::FILES_PAGE,
        )])
        .await;
        let client = CfClient::with_base(&base, None);
        // The fixture reports 53 files; the canned server answers every page
        // with the same 3, so two pages are read.
        let files = client.get_files(925200).await.unwrap();
        assert_eq!(files.len(), 6);
    }

    #[test]
    fn profile_endpoint_resolution_respects_env_and_keys() {
        use crate::config::profile::{
            ControllerKind, ControllerSection, OverlaySection, PackSection, SecretsSection,
            ServerSection,
        };
        use crate::providers::curseforge::endpoint::{ENV_API_BASE, ENV_PROXY_SEND_KEY};
        use crate::testutil::{EnvGuard, env_lock};
        use std::ffi::OsStr;
        use std::path::PathBuf;

        let _lock = env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("PACKCTL_HOME", home.path().as_os_str());
        let _base = EnvGuard::set(ENV_API_BASE, OsStr::new(""));
        let _send = EnvGuard::set(ENV_PROXY_SEND_KEY, OsStr::new(""));
        let _key = EnvGuard::set("CF_API_KEY", OsStr::new(""));

        let profile = ServerProfile {
            name: "t".to_string(),
            server: ServerSection {
                root: PathBuf::from("/srv"),
            },
            pack: PackSection::curseforge(1),
            overlay: OverlaySection {
                path: PathBuf::from("/srv/overlay"),
            },
            controller: ControllerSection {
                kind: ControllerKind::Command,
                instance: None,
                command: None,
            },
            secrets: SecretsSection::default(),
        };

        // No key anywhere: the community proxy, no key header.
        let client = CfClient::for_profile(&profile).unwrap();
        assert_eq!(client.endpoint().mode, CfEndpointMode::Proxy);
        assert_eq!(header_key(&client), None);

        // CF_API_KEY selects the official API and is sent there.
        let _env_key = EnvGuard::set("CF_API_KEY", OsStr::new("env-key"));
        let client = CfClient::for_profile(&profile).unwrap();
        assert_eq!(client.endpoint().mode, CfEndpointMode::Official);
        assert_eq!(header_key(&client).as_deref(), Some("env-key"));

        // A custom base does not get the key unless explicitly allowed.
        let _custom = EnvGuard::set(ENV_API_BASE, OsStr::new("https://cf.example/"));
        let client = CfClient::for_profile(&profile).unwrap();
        assert_eq!(client.endpoint().base_url, "https://cf.example");
        assert_eq!(header_key(&client), None);
        let _allow = EnvGuard::set(ENV_PROXY_SEND_KEY, OsStr::new("1"));
        let client = CfClient::for_profile(&profile).unwrap();
        assert_eq!(header_key(&client).as_deref(), Some("env-key"));
    }
}
