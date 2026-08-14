//! HTTP client for the CurseForge API (api.curseforge.com, v1).
//!
//! The API key is stored only inside this client and is sent as the
//! `x-api-key` header on every request. It is never logged or exposed.

use std::path::Path;
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, ClientBuilder, StatusCode};
use tokio::io::AsyncWriteExt;

use crate::error::{PackError, Result};

use super::models::{CfFile, CfFileListResponse, CfMod, CfModSearchResponse};

const DEFAULT_BASE_URL: &str = "https://api.curseforge.com";
const API_KEY_HEADER: &str = "x-api-key";
const USER_AGENT: &str = concat!("packctl/", env!("CARGO_PKG_VERSION"));
const PAGE_SIZE: u64 = 50;
const MAX_PAGES: u32 = 200;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Game id for Minecraft.
const GAME_ID_MINECRAFT: u32 = 432;

/// HTTP client for the CurseForge API.
pub struct CfClient {
    http: Client,
    api_key: Option<String>,
    base_url: String,
    /// Bound on concurrent downloads issued through this client.
    pub max_concurrent_downloads: usize,
}

impl CfClient {
    /// Builds a client from the `CF_API_KEY` environment variable.
    ///
    /// The key may be absent; public API reads work without one.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("CF_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty());
        Ok(Self::with_api_key(api_key))
    }

    /// Builds a client with an explicit API key (useful for tests).
    pub fn with_api_key(api_key: Option<String>) -> Self {
        let http = ClientBuilder::new()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .default_headers(request_headers(&api_key))
            .build()
            .expect("reqwest client construction cannot fail with the configured defaults");
        Self {
            http,
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            max_concurrent_downloads: 4,
        }
    }

    /// True when an API key is configured.
    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// Fetches the mod with `project_id`.
    pub async fn get_mod(&self, project_id: u32) -> Result<CfMod> {
        let url = self.mod_url(project_id);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| PackError::Network(format!("failed to fetch {url}: {e}")))?;
        let response = self.ensure_success(response, &url).await?;
        response
            .json()
            .await
            .map_err(|e| PackError::Parse(format!("failed to parse mod payload from {url}: {e}")))
    }

    /// Fetches every file of the mod with `project_id`, paging through all pages.
    pub async fn get_files(&self, project_id: u32) -> Result<Vec<CfFile>> {
        let mut files = Vec::new();
        let mut index: u64 = 0;
        for _page in 0..MAX_PAGES {
            let url = self.files_page_url(project_id, index);
            let response = self
                .http
                .get(&url)
                .send()
                .await
                .map_err(|e| PackError::Network(format!("failed to fetch {url}: {e}")))?;
            let response = self.ensure_success(response, &url).await?;
            let page: CfFileListResponse = response.json().await.map_err(|e| {
                PackError::Parse(format!("failed to parse file list payload from {url}: {e}"))
            })?;
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

    /// Fetches a single file belonging to the mod with `project_id`.
    pub async fn get_file(&self, project_id: u32, file_id: u32) -> Result<CfFile> {
        let url = self.file_url(project_id, file_id);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| PackError::Network(format!("failed to fetch {url}: {e}")))?;
        let response = self.ensure_success(response, &url).await?;
        response
            .json()
            .await
            .map_err(|e| PackError::Parse(format!("failed to parse file payload from {url}: {e}")))
    }

    /// Fetches the first Minecraft project whose slug matches `slug`.
    ///
    /// The CurseForge search endpoint filters by slug; public reads work
    /// without an API key.
    pub async fn search_by_slug(&self, slug: &str) -> Result<CfMod> {
        let url = self.search_endpoint();
        let response = self
            .http
            .get(&url)
            .query(&[
                ("gameId", GAME_ID_MINECRAFT.to_string()),
                ("slug", slug.to_string()),
            ])
            .send()
            .await
            .map_err(|e| PackError::Network(format!("failed to fetch {url}: {e}")))?;
        let response = self.ensure_success(response, &url).await?;
        let payload: CfModSearchResponse = response.json().await.map_err(|e| {
            PackError::Parse(format!("failed to parse search payload from {url}: {e}"))
        })?;
        payload.data.into_iter().next().ok_or_else(|| {
            PackError::NotFound(format!("no CurseForge project found for slug '{slug}'"))
        })
    }

    /// Streams `url` to `dest`, creating parent directories as needed.
    pub async fn download_to(&self, url: &str, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                PackError::io(format!("create directory '{}'", parent.display()), e)
            })?;
        }
        let response =
            self.http.get(url).send().await.map_err(|e| {
                PackError::Network(format!("failed to start download from {url}: {e}"))
            })?;
        let response = self.ensure_success(response, url).await?;

        let mut stream = response.bytes_stream();
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| PackError::io(format!("create '{}'", dest.display()), e))?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                PackError::Network(format!("download from {url} was interrupted: {e}"))
            })?;
            file.write_all(&chunk)
                .await
                .map_err(|e| PackError::io(format!("write '{}'", dest.display()), e))?;
        }
        file.flush()
            .await
            .map_err(|e| PackError::io(format!("flush '{}'", dest.display()), e))?;
        Ok(())
    }

    fn mod_url(&self, project_id: u32) -> String {
        format!("{}/v1/mods/{}", self.base_url, project_id)
    }

    /// URL of the file list endpoint for `project_id`.
    pub(crate) fn files_url(&self, project_id: u32) -> String {
        format!("{}/v1/mods/{}/files", self.base_url, project_id)
    }

    fn file_url(&self, project_id: u32, file_id: u32) -> String {
        format!("{}/v1/mods/{}/files/{}", self.base_url, project_id, file_id)
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
        format!("{}/v1/mods/search", self.base_url)
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
        let mut body = response.text().await.unwrap_or_default();
        body.truncate(256);
        let reason = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                "authentication failed; set CF_API_KEY (get a free key at \
                 https://console.curseforge.com/)"
                    .to_string()
            }
            StatusCode::NOT_FOUND => "resource not found".to_string(),
            StatusCode::TOO_MANY_REQUESTS => "rate limited; retry later".to_string(),
            _ => "unexpected status".to_string(),
        };
        let detail = if body.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", body.trim())
        };
        Err(PackError::Network(format!(
            "request to {url} returned HTTP {} ({reason}){detail}",
            status.as_u16()
        )))
    }
}

fn request_headers(api_key: &Option<String>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(key) = api_key
        && let Ok(value) = HeaderValue::from_str(key.trim())
    {
        headers.insert(API_KEY_HEADER, value);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> CfClient {
        CfClient::with_api_key(None)
    }

    #[test]
    fn mod_url_includes_project_id() {
        let client = test_client();
        assert_eq!(
            client.mod_url(925200),
            "https://api.curseforge.com/v1/mods/925200"
        );
    }

    #[test]
    fn files_url_includes_project_id() {
        let client = test_client();
        assert_eq!(
            client.files_url(925200),
            "https://api.curseforge.com/v1/mods/925200/files"
        );
    }

    #[test]
    fn file_url_includes_project_and_file_id() {
        let client = test_client();
        assert_eq!(
            client.file_url(925200, 42),
            "https://api.curseforge.com/v1/mods/925200/files/42"
        );
    }

    #[test]
    fn files_page_url_includes_pagination_params() {
        let client = test_client();
        assert_eq!(
            client.files_page_url(925200, 50),
            "https://api.curseforge.com/v1/mods/925200/files?pageSize=50&index=50"
        );
    }

    #[test]
    fn search_endpoint_is_mod_search() {
        let client = test_client();
        assert_eq!(
            client.search_endpoint(),
            "https://api.curseforge.com/v1/mods/search"
        );
    }

    #[test]
    fn request_headers_include_api_key_when_set() {
        let headers = request_headers(&Some("secret-key".to_string()));
        assert_eq!(
            headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok()),
            Some("secret-key")
        );
    }

    #[test]
    fn request_headers_omit_api_key_when_absent() {
        let headers = request_headers(&None);
        assert!(headers.get(API_KEY_HEADER).is_none());
    }

    #[test]
    fn request_headers_trim_whitespace() {
        let headers = request_headers(&Some("  spaced-key  ".to_string()));
        assert_eq!(
            headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok()),
            Some("spaced-key")
        );
    }

    #[test]
    fn from_env_reads_cf_api_key() {
        unsafe {
            std::env::set_var("CF_API_KEY", "env-key");
        }
        let client = CfClient::from_env().unwrap();
        assert!(client.has_api_key());
        unsafe {
            std::env::remove_var("CF_API_KEY");
        }
        let client = CfClient::from_env().unwrap();
        assert!(!client.has_api_key());
    }
}
