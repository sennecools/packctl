//! Which CurseForge API endpoint packctl talks to, and whether the user's
//! API key may be sent there.
//!
//! Resolution, highest priority first:
//! 1. `$PACKCTL_CF_API_BASE`;
//! 2. the profile's `[pack] api_base`;
//! 3. the official API, `https://api.curseforge.com`, when an API key is
//!    configured (`$CF_API_KEY`, the profile's stored key, or the shared
//!    global key);
//! 4. otherwise the community proxy [`DEFAULT_PROXY_BASE_URL`].
//!
//! The key is sent only to the official API, or to a custom endpoint when
//! `$PACKCTL_CF_PROXY_SEND_KEY=1`. It is never sent to the community proxy.

use std::fmt;

use reqwest::Url;

use crate::error::{PackError, Result};

/// The official CurseForge API.
pub const OFFICIAL_BASE_URL: &str = "https://api.curseforge.com";

/// The community proxy used when no API key is configured.
///
/// api.curse.tools is an anonymous, third-party, caching mirror of the
/// official v1 API (responses may lag by up to an hour). It is not affiliated
/// with CurseForge, and using it is a grey area under the CurseForge 3rd-party
/// API terms section 3.1 (<https://support.curseforge.com/en/support/solutions/articles/9000207405-curse-forge-3rd-party-api-terms-and-conditions>).
/// A personal key from <https://console.curseforge.com/> is the recommended
/// setup for production servers.
///
/// Deliberately, packctl has no second fallback proxy, and it never embeds or
/// borrows another application's API key: section 2.2 of the same terms
/// forbids sharing keys.
pub const DEFAULT_PROXY_BASE_URL: &str = "https://api.curse.tools";

/// Overrides the endpoint for every profile.
pub const ENV_API_BASE: &str = "PACKCTL_CF_API_BASE";

/// `1` sends the user's API key to a custom (non-official) endpoint too.
pub const ENV_PROXY_SEND_KEY: &str = "PACKCTL_CF_PROXY_SEND_KEY";

/// The host of the official API.
const OFFICIAL_HOST: &str = "api.curseforge.com";

/// What kind of endpoint a base URL is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfEndpointMode {
    /// api.curseforge.com over https.
    Official,
    /// The default community proxy.
    Proxy,
    /// Any other base URL (a self-hosted proxy, a test server).
    Custom,
}

/// A resolved CurseForge endpoint. Holds the API key only when it may be
/// sent to this endpoint.
#[derive(Clone)]
pub struct CfEndpoint {
    /// Normalized base URL without a trailing slash or `/v1`.
    pub base_url: String,
    pub mode: CfEndpointMode,
    /// The key, present only when it may be sent to `base_url`.
    api_key: Option<String>,
    /// Whether the user has a key configured at all (sent or not).
    pub user_key_configured: bool,
}

impl fmt::Debug for CfEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CfEndpoint")
            .field("base_url", &self.base_url)
            .field("mode", &self.mode)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("user_key_configured", &self.user_key_configured)
            .finish()
    }
}

impl CfEndpoint {
    /// The API key to send to this endpoint, if any.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// A one-line, secret-free description for summaries and `doctor`.
    pub fn describe(&self) -> String {
        match self.mode {
            CfEndpointMode::Official if self.api_key.is_some() => {
                "official CurseForge API (api.curseforge.com) with your API key".to_string()
            }
            CfEndpointMode::Official => {
                "official CurseForge API without an API key (requests will be rejected)".to_string()
            }
            CfEndpointMode::Proxy if self.user_key_configured => {
                format!("community proxy {DEFAULT_PROXY_BASE_URL} (your API key is not sent)")
            }
            CfEndpointMode::Proxy => {
                format!("community proxy {DEFAULT_PROXY_BASE_URL} (no API key configured)")
            }
            CfEndpointMode::Custom => format!(
                "custom endpoint {} (API key {})",
                self.base_url,
                if self.api_key.is_some() {
                    "sent"
                } else {
                    "not sent"
                }
            ),
        }
    }
}

/// Picks the endpoint and decides whether `key` may be sent to it.
///
/// `env_base` wins over `profile_base` (blank values count as unset); with
/// neither, a configured key selects the official API and no key selects the
/// community proxy.
pub fn resolve_endpoint(
    env_base: Option<&str>,
    profile_base: Option<&str>,
    key: Option<String>,
    send_key_to_custom: bool,
) -> Result<CfEndpoint> {
    let key = key
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty());
    let configured = [env_base, profile_base]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|base| !base.is_empty());
    let base_url = match configured {
        Some(base) => normalize_base(base)?,
        None if key.is_some() => OFFICIAL_BASE_URL.to_string(),
        None => DEFAULT_PROXY_BASE_URL.to_string(),
    };
    let mode = mode_of(&base_url);
    let send = match mode {
        CfEndpointMode::Official => true,
        CfEndpointMode::Proxy => false,
        CfEndpointMode::Custom => send_key_to_custom,
    };
    Ok(CfEndpoint {
        base_url,
        mode,
        user_key_configured: key.is_some(),
        api_key: if send { key } else { None },
    })
}

/// Classifies a normalized base URL by parsing it, never by string prefix,
/// so `https://api.curseforge.com.evil.example` is not official.
fn mode_of(base_url: &str) -> CfEndpointMode {
    let Ok(url) = Url::parse(base_url) else {
        return CfEndpointMode::Custom;
    };
    if url.scheme() == "https" && url.host_str() == Some(OFFICIAL_HOST) {
        CfEndpointMode::Official
    } else if base_url == DEFAULT_PROXY_BASE_URL {
        CfEndpointMode::Proxy
    } else {
        CfEndpointMode::Custom
    }
}

/// Validates and normalizes a user-supplied API base.
///
/// Trims, strips trailing slashes and one trailing `/v1`, and requires
/// `https://` (or `http://` for localhost). User info, queries and fragments
/// are rejected so credentials never end up in output, as is the legacy
/// `/v1/cf` proxy layout (it redirects to plain http).
pub fn normalize_base(raw: &str) -> Result<String> {
    let mut base = raw.trim().trim_end_matches('/');
    if let Some(stripped) = base.strip_suffix("/v1") {
        base = stripped.trim_end_matches('/');
    }
    let invalid = |shown: &str, reason: &str| {
        PackError::Config(format!("invalid CurseForge API base '{shown}': {reason}"))
    };
    let url = Url::parse(base).map_err(|e| invalid(raw.trim(), &e.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        let mut redacted = url.clone();
        let _ = redacted.set_username("");
        let _ = redacted.set_password(None);
        return Err(invalid(
            redacted.as_str(),
            "credentials in the URL are not supported",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid(raw.trim(), "a query or fragment is not supported"));
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| invalid(raw.trim(), "the URL has no host"))?;
    match url.scheme() {
        "https" => {}
        "http" if matches!(host, "localhost" | "127.0.0.1" | "[::1]") => {}
        _ => {
            return Err(invalid(
                raw.trim(),
                "use https:// (http:// is allowed only for localhost)",
            ));
        }
    }
    if url.path().to_ascii_lowercase().contains("/v1/cf") {
        return Err(invalid(
            raw.trim(),
            "use https://api.curse.tools, not the legacy /v1/cf path",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str) -> Option<String> {
        Some(k.to_string())
    }

    #[test]
    fn resolve_table() {
        struct Case {
            env: Option<&'static str>,
            profile: Option<&'static str>,
            key: Option<String>,
            send: bool,
            base: &'static str,
            mode: CfEndpointMode,
            sent: bool,
        }
        let cases = [
            // The environment wins over the profile.
            Case {
                env: Some("https://env.example"),
                profile: Some("https://profile.example"),
                key: None,
                send: false,
                base: "https://env.example",
                mode: CfEndpointMode::Custom,
                sent: false,
            },
            // A blank environment value counts as unset; the profile is next.
            Case {
                env: Some("  "),
                profile: Some("https://profile.example/"),
                key: None,
                send: false,
                base: "https://profile.example",
                mode: CfEndpointMode::Custom,
                sent: false,
            },
            // A key selects the official API and is sent there.
            Case {
                env: None,
                profile: None,
                key: key("k"),
                send: false,
                base: OFFICIAL_BASE_URL,
                mode: CfEndpointMode::Official,
                sent: true,
            },
            // No key selects the community proxy.
            Case {
                env: None,
                profile: None,
                key: None,
                send: false,
                base: DEFAULT_PROXY_BASE_URL,
                mode: CfEndpointMode::Proxy,
                sent: false,
            },
            // The key is never sent to the proxy, even with send-key set.
            Case {
                env: Some("https://api.curse.tools/v1/"),
                profile: None,
                key: key("k"),
                send: true,
                base: DEFAULT_PROXY_BASE_URL,
                mode: CfEndpointMode::Proxy,
                sent: false,
            },
            // A custom base gets the key only with PACKCTL_CF_PROXY_SEND_KEY.
            Case {
                env: Some("https://cf.example/api"),
                profile: None,
                key: key("k"),
                send: false,
                base: "https://cf.example/api",
                mode: CfEndpointMode::Custom,
                sent: false,
            },
            Case {
                env: Some("https://cf.example/api"),
                profile: None,
                key: key("k"),
                send: true,
                base: "https://cf.example/api",
                mode: CfEndpointMode::Custom,
                sent: true,
            },
            // The official API chosen explicitly without a key.
            Case {
                env: None,
                profile: Some("https://api.curseforge.com/v1"),
                key: None,
                send: false,
                base: OFFICIAL_BASE_URL,
                mode: CfEndpointMode::Official,
                sent: false,
            },
            // A blank key is no key.
            Case {
                env: None,
                profile: None,
                key: key("   "),
                send: false,
                base: DEFAULT_PROXY_BASE_URL,
                mode: CfEndpointMode::Proxy,
                sent: false,
            },
        ];
        for (index, case) in cases.into_iter().enumerate() {
            let had_key = case.key.as_deref().is_some_and(|k| !k.trim().is_empty());
            let endpoint = resolve_endpoint(case.env, case.profile, case.key, case.send).unwrap();
            assert_eq!(endpoint.base_url, case.base, "case {index}");
            assert_eq!(endpoint.mode, case.mode, "case {index}");
            assert_eq!(endpoint.api_key().is_some(), case.sent, "case {index}");
            assert_eq!(endpoint.user_key_configured, had_key, "case {index}");
        }
    }

    #[test]
    fn describe_names_the_endpoint_and_key_handling() {
        let describe = |env: Option<&str>, key: Option<String>, send: bool| {
            resolve_endpoint(env, None, key, send).unwrap().describe()
        };
        assert_eq!(
            describe(None, key("k"), false),
            "official CurseForge API (api.curseforge.com) with your API key"
        );
        assert_eq!(
            describe(Some(OFFICIAL_BASE_URL), None, false),
            "official CurseForge API without an API key (requests will be rejected)"
        );
        assert_eq!(
            describe(None, None, false),
            "community proxy https://api.curse.tools (no API key configured)"
        );
        assert_eq!(
            describe(Some("https://cf.example"), key("k"), true),
            "custom endpoint https://cf.example (API key sent)"
        );
        assert_eq!(
            describe(Some("https://cf.example"), key("k"), false),
            "custom endpoint https://cf.example (API key not sent)"
        );
    }

    #[test]
    fn normalize_base_strips_slash_and_v1() {
        for (raw, expected) in [
            ("https://api.curse.tools", "https://api.curse.tools"),
            ("  https://api.curse.tools/  ", "https://api.curse.tools"),
            ("https://api.curse.tools/v1", "https://api.curse.tools"),
            ("https://api.curse.tools/v1/", "https://api.curse.tools"),
            ("https://API.CurseForge.com//", "https://api.curseforge.com"),
            ("https://cf.example/proxy/v1", "https://cf.example/proxy"),
            ("https://cf.example:8443/", "https://cf.example:8443"),
        ] {
            assert_eq!(normalize_base(raw).unwrap(), expected, "raw: {raw}");
        }
    }

    #[test]
    fn normalize_base_rejects_http_non_localhost() {
        for raw in [
            "http://api.curse.tools",
            "http://api.curseforge.com",
            "ftp://cf.example",
        ] {
            let err = normalize_base(raw).unwrap_err();
            assert!(
                matches!(&err, PackError::Config(m)
                    if m.starts_with(&format!("invalid CurseForge API base '{raw}': "))),
                "{err:?}"
            );
        }
    }

    #[test]
    fn normalize_base_accepts_http_localhost() {
        for (raw, expected) in [
            ("http://localhost:8080/", "http://localhost:8080"),
            ("http://127.0.0.1:9000/v1", "http://127.0.0.1:9000"),
            ("http://[::1]:9000", "http://[::1]:9000"),
        ] {
            assert_eq!(normalize_base(raw).unwrap(), expected, "raw: {raw}");
        }
    }

    #[test]
    fn normalize_base_rejects_legacy_v1_cf() {
        for raw in [
            "https://api.curse.tools/v1/cf",
            "https://api.curse.tools/v1/cf/",
            "https://api.curse.tools/V1/CF/v1",
        ] {
            let err = normalize_base(raw).unwrap_err();
            assert!(
                err.to_string()
                    .ends_with("use https://api.curse.tools, not the legacy /v1/cf path"),
                "{err}"
            );
        }
    }

    #[test]
    fn normalize_base_rejects_userinfo_query_and_fragment_without_echoing_secrets() {
        let err = normalize_base("https://user:hunter2@cf.example").unwrap_err();
        let message = err.to_string();
        assert!(!message.contains("hunter2"), "{message}");
        assert!(!message.contains("user"), "{message}");
        assert!(message.contains("credentials"), "{message}");

        assert!(normalize_base("https://cf.example/?token=1").is_err());
        assert!(normalize_base("https://cf.example/#frag").is_err());
        assert!(normalize_base("not a url").is_err());
        assert!(normalize_base("https://").is_err());
    }

    #[test]
    fn official_mode_requires_the_real_host_over_https() {
        for (raw, mode) in [
            ("https://api.curseforge.com", CfEndpointMode::Official),
            ("https://api.curseforge.com:443/", CfEndpointMode::Official),
            (
                "https://api.curseforge.com.evil.example",
                CfEndpointMode::Custom,
            ),
            (
                "https://evil.example/api.curseforge.com",
                CfEndpointMode::Custom,
            ),
            ("https://api.curse.tools", CfEndpointMode::Proxy),
            ("https://api.curse.tools/mirror", CfEndpointMode::Custom),
        ] {
            let endpoint = resolve_endpoint(Some(raw), None, key("k"), false).unwrap();
            assert_eq!(endpoint.mode, mode, "raw: {raw}");
            assert_eq!(
                endpoint.api_key().is_some(),
                mode == CfEndpointMode::Official,
                "raw: {raw}"
            );
        }
        // Userinfo that would smuggle the official host is rejected outright.
        assert!(
            resolve_endpoint(
                Some("https://api.curseforge.com@evil.example"),
                None,
                None,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn debug_output_redacts_key() {
        let endpoint = resolve_endpoint(None, None, key("super-secret-key"), false).unwrap();
        let debug = format!("{endpoint:?}");
        assert!(!debug.contains("super-secret-key"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!endpoint.describe().contains("super-secret-key"));
    }
}
