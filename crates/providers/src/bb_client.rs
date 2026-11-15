//! Bitbucket Cloud layer over the shared `ApiClient`: Basic `email:token` auth, Bitbucket's
//! `error.message` refusal text and `next`-field paging with `pagelen`.

use std::fmt;
use std::time::Duration;

use crate::http::{ApiClient, AuthScheme, ClientOpts, CONNECT_TIMEOUT, DEFAULT_TIMEOUT};
use crate::provider::ProviderError;

pub use crate::http::CERT_UNTRUSTED;

const PAGE_LEN: &str = "100";
/// A longer chain of `next` links is treated as a paging loop.
const MAX_PAGES: usize = 200;

/// Bitbucket Cloud HTTP client. Credentials are held only by the inner `ApiClient` as a
/// sensitive header value and are never printed by `Debug`.
pub struct BbClient {
    api: ApiClient,
    email: String,
}

impl fmt::Debug for BbClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BbClient")
            .field("base_url", &self.api.base_url())
            .field("email", &self.email)
            .field("token", &"***")
            .finish()
    }
}

fn bitbucket_error_message(v: &serde_json::Value) -> Option<&str> {
    v["error"]["message"].as_str()
}

impl BbClient {
    pub fn new(base_url: &str, email: &str, token: &str) -> BbClient {
        Self::with_timeout(base_url, email, token, DEFAULT_TIMEOUT)
    }

    /// `new` with another per-request timeout (tests of a request that never answers).
    pub fn with_timeout(base_url: &str, email: &str, token: &str, timeout: Duration) -> BbClient {
        Self::with_timeouts(base_url, email, token, timeout, CONNECT_TIMEOUT)
    }

    /// `with_timeout` with its own connect timeout. A connect timeout is both `is_timeout()`
    /// and `is_connect()`: nothing was sent, so it stays a definite "not posted".
    pub fn with_timeouts(
        base_url: &str,
        email: &str,
        token: &str,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> BbClient {
        let api = ApiClient::new(
            base_url,
            AuthScheme::Basic {
                user: email.to_string(),
                token: token.to_string(),
            },
            ClientOpts {
                timeout,
                connect_timeout,
                error_message: bitbucket_error_message,
                extra_headers: Vec::new(),
                retry_429: true,
            },
        );
        BbClient {
            api,
            email: email.to_string(),
        }
    }

    /// The shared client underneath (same origin, redirect and retry rules).
    pub fn api(&self) -> &ApiClient {
        &self.api
    }

    /// GET a path (relative to the base URL) or an absolute URL on the same origin.
    pub async fn get_json(&self, path_or_url: &str) -> Result<serde_json::Value, ProviderError> {
        self.api.get_json(path_or_url).await
    }

    /// GET a path and return the raw body (for non-JSON endpoints such as a PR `/diff`).
    pub async fn get_text(&self, path_or_url: &str) -> Result<String, ProviderError> {
        self.api.get_text(path_or_url).await
    }

    /// POST a JSON body. **Prototype-only write path** (not used by the app): returns
    /// `(status, body)` for every non-429 response, because the experiment needs to see
    /// Bitbucket's rejection payloads.
    pub async fn post_json(
        &self,
        path_or_url: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), ProviderError> {
        self.api.post_json(path_or_url, body).await
    }

    /// POST (optionally with a JSON body) and require a 2xx. A refusal may carry
    /// Bitbucket's message (`Rejected`).
    pub async fn post_checked(
        &self,
        path_or_url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, ProviderError> {
        self.api.post_checked(path_or_url, body).await
    }

    /// DELETE and require a 2xx (204 included).
    pub async fn delete_checked(
        &self,
        path_or_url: &str,
    ) -> Result<serde_json::Value, ProviderError> {
        self.api.delete_checked(path_or_url).await
    }

    /// POST that creates something (a comment); see `ApiClient::post_create`.
    pub async fn post_create(
        &self,
        path_or_url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ProviderError> {
        self.api.post_create(path_or_url, body).await
    }

    /// GET a single page of a paginated collection: its `values` and the `next` URL, if any.
    /// The path is used as given (no default `pagelen` is added).
    pub async fn get_page(
        &self,
        path_or_url: &str,
    ) -> Result<(Vec<serde_json::Value>, Option<String>), ProviderError> {
        let page = self.get_json(path_or_url).await?;
        let values = page
            .get("values")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let next = page
            .get("next")
            .and_then(|n| n.as_str())
            .map(str::to_string);
        Ok((values, next))
    }

    /// GET every page of a paginated collection and concatenate the `values`.
    pub async fn get_all(&self, path: &str) -> Result<Vec<serde_json::Value>, ProviderError> {
        let mut next = Some(with_pagelen(path));
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while let Some(url) = next.take() {
            if seen.len() >= MAX_PAGES || !seen.insert(url.clone()) {
                return Err(ProviderError::Decode("paging loop".into()));
            }
            let (values, n) = self.get_page(&url).await?;
            out.extend(values);
            next = n;
        }
        Ok(out)
    }
}

fn with_pagelen(path: &str) -> String {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    if query
        .split('&')
        .any(|kv| kv.split('=').next() == Some("pagelen"))
    {
        return path.to_string();
    }
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}pagelen={PAGE_LEN}")
}

#[cfg(test)]
mod tests;
