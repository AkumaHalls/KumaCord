//! The only code in the hub that talks to arbitrary web servers.
//!
//! Rules: http(s) only; no cookies, no proxy, no compression (so no decompression bombs); every
//! hop (first request and each redirect, at most 3) is checked by the [`Policy`] and connects
//! only to addresses the [`SafeResolver`] let through; one hard deadline for the whole fetch;
//! and a byte cap per kind of resource (HTML is read only up to `</head>`).

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderValue, ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, LOCATION};
use reqwest::redirect;
use reqwest::Url;

use super::ssrf::{Policy, SafeResolver};

const USER_AGENT: &str = "Mozilla/5.0 (compatible; concord-link-preview/1.0)";

#[derive(Debug, Clone)]
pub struct Limits {
    /// Most bytes read from an HTML page (the scan stops at `</head>` sooner).
    pub html_cap: usize,
    pub image_cap: usize,
    pub json_cap: usize,
    /// Deadline for one whole fetch, redirects included.
    pub timeout: Duration,
    pub max_redirects: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            html_cap: 1024 * 1024,
            image_cap: 5 * 1024 * 1024,
            json_cap: 64 * 1024,
            timeout: Duration::from_secs(5),
            max_redirects: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Html,
    Image,
    Json,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The policy refused the URL (scheme, credentials, private address...).
    Blocked,
    Timeout,
    Status(u16),
    BadType,
    TooLarge,
    TooManyRedirects,
    Network,
}

#[derive(Debug)]
pub struct Fetched {
    /// Where the body really came from (after redirects); relative URLs resolve against it.
    pub url: Url,
    pub content_type: String,
    pub body: Vec<u8>,
}

pub struct Fetcher {
    client: reqwest::Client,
    policy: Arc<Policy>,
    limits: Limits,
}

/// reqwest's rustls needs a process-wide crypto provider when more than one is compiled in
/// (a workspace build also has the client's aws-lc). Ring is the one the hub itself uses.
pub(crate) fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl Fetcher {
    pub fn new(policy: Arc<Policy>, limits: Limits) -> anyhow::Result<Self> {
        install_crypto_provider();
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(SafeResolver(policy.clone())))
            .connect_timeout(Duration::from_secs(3))
            .read_timeout(Duration::from_secs(4))
            .pool_idle_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Fetcher { client, policy, limits })
    }

    pub fn policy(&self) -> &Arc<Policy> {
        &self.policy
    }

    pub async fn get(&self, url: &Url, kind: Kind) -> Result<Fetched, FetchError> {
        match tokio::time::timeout(self.limits.timeout, self.get_inner(url.clone(), kind)).await {
            Ok(r) => r,
            Err(_) => Err(FetchError::Timeout),
        }
    }

    async fn get_inner(&self, mut url: Url, kind: Kind) -> Result<Fetched, FetchError> {
        let accept = match kind {
            Kind::Html => "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1",
            Kind::Image => "image/jpeg,image/png,image/webp,image/gif,image/*;q=0.5",
            Kind::Json => "application/json",
        };
        for _hop in 0..=self.limits.max_redirects {
            self.policy.check_url(&url).map_err(|_| FetchError::Blocked)?;
            let mut resp = self
                .client
                .get(url.clone())
                .header(ACCEPT, accept)
                .header(ACCEPT_LANGUAGE, HeaderValue::from_static("pt-BR,pt;q=0.9,en;q=0.8"))
                .send()
                .await
                .map_err(|_| FetchError::Network)?;
            let status = resp.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let next = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|loc| url.join(loc).ok())
                    .ok_or(FetchError::Network)?;
                url = next;
                continue;
            }
            if !status.is_success() {
                return Err(FetchError::Status(status.as_u16()));
            }
            let content_type = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !type_ok(kind, &content_type) {
                return Err(FetchError::BadType);
            }
            let cap = match kind {
                Kind::Html => self.limits.html_cap,
                Kind::Image => self.limits.image_cap,
                Kind::Json => self.limits.json_cap,
            };
            if kind != Kind::Html && resp.content_length().is_some_and(|n| n > cap as u64) {
                return Err(FetchError::TooLarge);
            }
            let mut body: Vec<u8> = Vec::new();
            loop {
                let Some(chunk) = resp.chunk().await.map_err(|_| FetchError::Network)? else { break };
                let scan_from = body.len().saturating_sub(7);
                body.extend_from_slice(&chunk);
                if kind == Kind::Html {
                    if body[scan_from..].windows(7).any(|w| w.eq_ignore_ascii_case(b"</head>") || w.eq_ignore_ascii_case(b"</head ")) {
                        break;
                    }
                    if body.len() >= cap {
                        body.truncate(cap);
                        break;
                    }
                } else if body.len() > cap {
                    return Err(FetchError::TooLarge);
                }
            }
            return Ok(Fetched { url, content_type, body });
        }
        Err(FetchError::TooManyRedirects)
    }
}

fn type_ok(kind: Kind, ct: &str) -> bool {
    match kind {
        Kind::Html => ct.is_empty() || ct.contains("html") || ct.contains("xml"),
        Kind::Image => ct.is_empty() || ct.starts_with("image/") || ct.contains("octet-stream"),
        Kind::Json => ct.is_empty() || ct.contains("json") || ct.starts_with("text/"),
    }
}
