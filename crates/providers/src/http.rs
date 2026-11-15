//! Forge-neutral HTTP client shared by every provider.
//!
//! What it guarantees, whatever the forge:
//! - credentials only go to the configured origin: relative paths resolve against the base
//!   URL, absolute URLs must be same-origin, reads follow at most `MAX_REDIRECTS`
//!   same-origin redirects, writes follow none;
//! - only 429 is retried (bounded `Retry-After`), nothing else;
//! - transport and status errors map to fixed `ProviderError`s that never carry a URL,
//!   a body or a credential; a refused write may carry the server's one-line message;
//! - a comment POST whose answer was lost is `Unconfirmed`, never guessed;
//! - one connection pool per process (rustls with the system roots, system proxy), shared
//!   by every client with the default connect timeout.

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use reqwest::{redirect, Client, StatusCode, Url};

use crate::provider::ProviderError;

const MAX_ATTEMPTS: u32 = 5;
const MAX_RETRY_AFTER_SECS: u64 = 60;
const DEFAULT_RETRY_AFTER_SECS: u64 = 1;
const MAX_REDIRECTS: usize = 5;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bodies larger than this are not searched for an error message.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Longest server message shown, in characters.
const MAX_MESSAGE_CHARS: usize = 160;

/// How a client authenticates. The token never appears in `Debug`.
#[derive(Clone)]
pub enum AuthScheme {
    /// `Authorization: Basic base64(user:token)` (Bitbucket API tokens).
    Basic { user: String, token: String },
    /// `Authorization: Bearer token` (GitHub).
    Bearer(String),
    /// `PRIVATE-TOKEN: token` (GitLab).
    PrivateToken(String),
}

impl fmt::Debug for AuthScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthScheme::Basic { user, .. } => f
                .debug_struct("Basic")
                .field("user", user)
                .field("token", &"***")
                .finish(),
            AuthScheme::Bearer(_) => f.write_str("Bearer(***)"),
            AuthScheme::PrivateToken(_) => f.write_str("PrivateToken(***)"),
        }
    }
}

impl AuthScheme {
    /// The header carrying the credential, marked sensitive.
    fn header(&self) -> (HeaderName, HeaderValue) {
        let (name, value) = match self {
            AuthScheme::Basic { user, token } => {
                let encoded =
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
                (AUTHORIZATION, format!("Basic {encoded}"))
            }
            AuthScheme::Bearer(t) => (AUTHORIZATION, format!("Bearer {t}")),
            AuthScheme::PrivateToken(t) => (HeaderName::from_static("private-token"), t.clone()),
        };
        // A token with a control character cannot be sent; an empty sensitive value makes
        // the server answer 401 instead of the request failing to build.
        let mut value = HeaderValue::from_str(&value).unwrap_or(HeaderValue::from_static(""));
        value.set_sensitive(true);
        (name, value)
    }
}

/// Reads the forge's refusal message out of an error body (`None` when it has none).
pub type ErrorMessage = fn(&serde_json::Value) -> Option<&str>;

/// `message` at the top level or under `error` (covers GitHub, GitLab and Bitbucket shapes).
pub fn generic_error_message(v: &serde_json::Value) -> Option<&str> {
    v["error"]["message"]
        .as_str()
        .or_else(|| v["message"].as_str())
}

/// Per-client settings.
#[derive(Clone)]
pub struct ClientOpts {
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub error_message: ErrorMessage,
    /// Sent with every request (e.g. an API version header). Never credentials.
    pub extra_headers: Vec<(HeaderName, HeaderValue)>,
    /// Retry a 429 after `Retry-After` (bounded). False: a 429 is returned to the caller
    /// (a forge whose rate-limit rules need more than this, GitHub).
    pub retry_429: bool,
}

impl Default for ClientOpts {
    fn default() -> Self {
        ClientOpts {
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
            error_message: generic_error_message,
            extra_headers: Vec::new(),
            retry_429: true,
        }
    }
}

/// Largest body a raw request reads; a larger answer is a `Decode` error.
pub const MAX_RAW_BODY: usize = 32 * 1024 * 1024;

/// A response as received, for forge clients that classify statuses and headers
/// themselves. Never logged: the body may hold anything the server sent.
pub struct RawResponse {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
}

impl RawResponse {
    /// A header as text (None when absent or not visible ASCII).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn json(&self) -> Result<serde_json::Value, ProviderError> {
        serde_json::from_slice(&self.body)
            .map_err(|_| ProviderError::Decode("invalid JSON in response body".into()))
    }

    /// The server's one-line refusal message, when the body has one (`message`).
    pub fn message(&self) -> Option<String> {
        if self.body.len() > MAX_ERROR_BODY {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&self.body).ok()?;
        generic_error_message(&v)
            .map(one_line)
            .filter(|m| !m.is_empty())
    }
}

/// The two pools: reads follow bounded same-origin redirects, writes none (reqwest would
/// turn a redirected POST/DELETE into a GET).
#[derive(Clone)]
struct Pools {
    read: Client,
    write: Client,
}

fn build_pools(connect_timeout: Duration) -> Pools {
    let write = Client::builder()
        .connect_timeout(connect_timeout)
        // Writes are rare and never retried: a kept-alive connection the server has
        // already closed would fail after the request was sent, which reads as
        // "unconfirmed". A fresh connection per write has no such window.
        .pool_max_idle_per_host(0)
        .redirect(redirect::Policy::none())
        .build()
        .expect("static client configuration is valid");
    let read = Client::builder()
        .connect_timeout(connect_timeout)
        // The first URL of the chain is the request the client sent, which `resolve` made
        // same-origin with the base URL; every hop must stay on that origin.
        .redirect(redirect::Policy::custom(|attempt| {
            let first = attempt.previous().first();
            let allowed = attempt.previous().len() <= MAX_REDIRECTS
                && first.is_some_and(|o| same_origin(o, attempt.url()));
            if allowed {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .expect("static client configuration is valid");
    Pools { read, write }
}

/// The process-wide pools for the default connect timeout.
fn shared_pools() -> &'static Pools {
    static POOLS: OnceLock<Pools> = OnceLock::new();
    POOLS.get_or_init(|| build_pools(CONNECT_TIMEOUT))
}

/// HTTP client bound to one API origin and one credential.
pub struct ApiClient {
    pools: Pools,
    shared: bool,
    timeout: Duration,
    base_url: String,
    origin: Option<Url>,
    auth: AuthScheme,
    auth_header: (HeaderName, HeaderValue),
    error_message: ErrorMessage,
    extra_headers: Vec<(HeaderName, HeaderValue)>,
    retry_429: bool,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .finish()
    }
}

impl ApiClient {
    pub fn new(base_url: &str, auth: AuthScheme, opts: ClientOpts) -> ApiClient {
        let shared = opts.connect_timeout == CONNECT_TIMEOUT;
        let pools = if shared {
            shared_pools().clone()
        } else {
            build_pools(opts.connect_timeout)
        };
        let base_url = base_url.trim_end_matches('/').to_string();
        ApiClient {
            pools,
            shared,
            timeout: opts.timeout,
            origin: Url::parse(&base_url).ok(),
            base_url,
            auth_header: auth.header(),
            auth,
            error_message: opts.error_message,
            extra_headers: opts.extra_headers,
            retry_429: opts.retry_429,
        }
    }

    /// GET with extra request headers (e.g. `If-None-Match`); every status comes back as
    /// is (429 too when `retry_429` is off). Same origin and redirect rules as `get_json`.
    pub async fn get_raw(
        &self,
        path_or_url: &str,
        headers: &[(HeaderName, HeaderValue)],
    ) -> Result<RawResponse, ProviderError> {
        let resp = self
            .send_with(
                reqwest::Method::GET,
                path_or_url,
                None,
                headers,
                network_error,
            )
            .await?;
        raw(resp).await
    }

    /// POST of a JSON body that only reads (GraphQL queries): every status comes back as
    /// is. Follows no redirect (POST).
    pub async fn post_raw(
        &self,
        path_or_url: &str,
        body: &serde_json::Value,
    ) -> Result<RawResponse, ProviderError> {
        let resp = self
            .send_with(
                reqwest::Method::POST,
                path_or_url,
                Some(body),
                &[],
                network_error,
            )
            .await?;
        raw(resp).await
    }

    /// A write (POST/PUT/PATCH/DELETE) for forge clients that classify answers themselves:
    /// every status comes back as is. Follows no redirect and is never retried (429 too
    /// when `retry_429` is off). Whether it applied is never guessed:
    /// - not sent (connection refused, DNS): `Network`;
    /// - sent, then a timeout or a reset: `Unconfirmed`;
    /// - a 2xx whose body cannot be read: `PostedUnreadable`; a 5xx: `Unconfirmed`.
    pub async fn write_raw(
        &self,
        method: reqwest::Method,
        path_or_url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<RawResponse, ProviderError> {
        if method == reqwest::Method::GET {
            return Err(ProviderError::Invalid("a write cannot be a GET".into()));
        }
        let resp = self
            .send_with(method, path_or_url, body, &[], unconfirmed_unless_unsent)
            .await?;
        let status = resp.status();
        match raw(resp).await {
            Ok(r) => Ok(r),
            Err(_) if status.is_success() => Err(ProviderError::PostedUnreadable),
            Err(_) if status.is_server_error() => Err(ProviderError::Unconfirmed),
            Err(e) => Err(e),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// True when `url` is an absolute URL on this client's origin (scheme, host and port).
    pub fn is_same_origin(&self, url: &str) -> bool {
        Url::parse(url)
            .ok()
            .is_some_and(|u| self.origin.as_ref().is_some_and(|o| same_origin(o, &u)))
    }

    /// True when this client uses the process-wide connection pool.
    pub fn uses_shared_pool(&self) -> bool {
        self.shared
    }

    /// GET a path (relative to the base URL) or an absolute URL on the same origin.
    pub async fn get_json(&self, path_or_url: &str) -> Result<serde_json::Value, ProviderError> {
        let resp = self
            .send(reqwest::Method::GET, path_or_url, None, network_error)
            .await?;
        let resp = ok_or_status_error(resp)?;
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| ProviderError::Decode(decode_message(&e)))
    }

    /// GET and return the raw body (non-JSON endpoints). Same rules as `get_json`.
    pub async fn get_text(&self, path_or_url: &str) -> Result<String, ProviderError> {
        let resp = self
            .send(reqwest::Method::GET, path_or_url, None, network_error)
            .await?;
        let resp = ok_or_status_error(resp)?;
        resp.text()
            .await
            .map_err(|e| ProviderError::Decode(decode_message(&e)))
    }

    /// POST returning `(status, body)` for every non-429 answer (prototypes only: they need
    /// to see the server's refusals). A non-JSON body comes back as a JSON string.
    pub async fn post_json(
        &self,
        path_or_url: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), ProviderError> {
        let resp = self
            .send(
                reqwest::Method::POST,
                path_or_url,
                Some(body),
                network_error,
            )
            .await?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Decode(decode_message(&e)))?;
        let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        Ok((status, value))
    }

    /// POST (optionally with a JSON body) and require a 2xx. Returns the JSON body, or `Null`
    /// when it is empty, unreadable or not JSON (e.g. 204).
    pub async fn post_checked(
        &self,
        path_or_url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, ProviderError> {
        self.write_checked(reqwest::Method::POST, path_or_url, body)
            .await
    }

    /// PUT, same handling as `post_checked`.
    pub async fn put_checked(
        &self,
        path_or_url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, ProviderError> {
        self.write_checked(reqwest::Method::PUT, path_or_url, body)
            .await
    }

    /// DELETE and require a 2xx (204 included).
    pub async fn delete_checked(
        &self,
        path_or_url: &str,
    ) -> Result<serde_json::Value, ProviderError> {
        self.write_checked(reqwest::Method::DELETE, path_or_url, None)
            .await
    }

    async fn write_checked(
        &self,
        method: reqwest::Method,
        path_or_url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, ProviderError> {
        let resp = self.send(method, path_or_url, body, network_error).await?;
        let resp = self.write_status(resp).await?;
        let text = resp.text().await.unwrap_or_default();
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    /// POST that creates something (a comment) and returns its JSON echo. Whether it was
    /// created must never be guessed:
    /// - not sent (connection refused, DNS): `Network`, definitely not created;
    /// - sent, then a timeout, a reset or a 5xx: `Unconfirmed`, it may exist;
    /// - 2xx whose body cannot be read or decoded: `PostedUnreadable`, it exists;
    /// - 4xx: a definite refusal (`Rejected` with the server's message when it gives one).
    pub async fn post_create(
        &self,
        path_or_url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ProviderError> {
        let resp = self
            .send(
                reqwest::Method::POST,
                path_or_url,
                Some(body),
                unconfirmed_unless_unsent,
            )
            .await?;
        if resp.status().is_server_error() {
            return Err(ProviderError::Unconfirmed);
        }
        let resp = self.write_status(resp).await?;
        let text = resp
            .text()
            .await
            .map_err(|_| ProviderError::PostedUnreadable)?;
        serde_json::from_str(&text).map_err(|_| ProviderError::PostedUnreadable)
    }

    /// Send with auth, same-origin resolution and 429 backoff. Returns the first non-429
    /// response; the caller maps its status. `on_err` classifies transport errors.
    async fn send(
        &self,
        method: reqwest::Method,
        path_or_url: &str,
        body: Option<&serde_json::Value>,
        on_err: fn(reqwest::Error) -> ProviderError,
    ) -> Result<reqwest::Response, ProviderError> {
        self.send_with(method, path_or_url, body, &[], on_err).await
    }

    async fn send_with(
        &self,
        method: reqwest::Method,
        path_or_url: &str,
        body: Option<&serde_json::Value>,
        headers: &[(HeaderName, HeaderValue)],
        on_err: fn(reqwest::Error) -> ProviderError,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = self.resolve(path_or_url)?;
        let client = if method == reqwest::Method::GET {
            &self.pools.read
        } else {
            &self.pools.write
        };
        for attempt in 1..=MAX_ATTEMPTS {
            let mut req = client
                .request(method.clone(), url.clone())
                .timeout(self.timeout)
                .header(self.auth_header.0.clone(), self.auth_header.1.clone())
                .header(USER_AGENT, "nit/0.1");
            for (k, v) in self.extra_headers.iter().chain(headers) {
                req = req.header(k.clone(), v.clone());
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(on_err)?;
            if resp.status() != StatusCode::TOO_MANY_REQUESTS || !self.retry_429 {
                return Ok(resp);
            }
            if attempt == MAX_ATTEMPTS {
                return Err(ProviderError::RateLimited);
            }
            let secs = resp
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
                .min(MAX_RETRY_AFTER_SECS);
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
        Err(ProviderError::RateLimited)
    }

    fn resolve(&self, path_or_url: &str) -> Result<Url, ProviderError> {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            let url = Url::parse(path_or_url)
                .map_err(|_| ProviderError::Network("invalid URL".into()))?;
            if !self.origin.as_ref().is_some_and(|o| same_origin(o, &url)) {
                return Err(ProviderError::Network(
                    "refusing to send credentials to a different origin".into(),
                ));
            }
            Ok(url)
        } else {
            let sep = if path_or_url.starts_with('/') {
                ""
            } else {
                "/"
            };
            Url::parse(&format!("{}{sep}{path_or_url}", self.base_url))
                .map_err(|_| ProviderError::Network("invalid URL".into()))
        }
    }

    /// Status mapping for writes: a refusal (400/403/404/409) carries the server's own
    /// message when the body has one; everything else maps like `ok_or_status_error`.
    async fn write_status(
        &self,
        resp: reqwest::Response,
    ) -> Result<reqwest::Response, ProviderError> {
        let status = resp.status().as_u16();
        if !matches!(status, 400 | 403 | 404 | 409) {
            return ok_or_status_error(resp);
        }
        let message = resp
            .bytes()
            .await
            .ok()
            .filter(|b| b.len() <= MAX_ERROR_BODY)
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| (self.error_message)(&v).map(one_line))
            .filter(|m| !m.is_empty());
        match (message, status) {
            (Some(message), _) => Err(ProviderError::Rejected { status, message }),
            (None, 403) => Err(ProviderError::Forbidden),
            (None, 404) => Err(ProviderError::NotFound),
            (None, code) => Err(ProviderError::Http(code)),
        }
    }
}

/// Reads a whole response, refusing bodies over `MAX_RAW_BODY`.
async fn raw(mut resp: reqwest::Response) -> Result<RawResponse, ProviderError> {
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| ProviderError::Decode(decode_message(&e)))?
    {
        if body.len() + chunk.len() > MAX_RAW_BODY {
            return Err(ProviderError::Decode("response body too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

fn ok_or_status_error(resp: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
    match resp.status().as_u16() {
        200..=299 => Ok(resp),
        401 => Err(ProviderError::Auth),
        403 => Err(ProviderError::Forbidden),
        404 => Err(ProviderError::NotFound),
        code => Err(ProviderError::Http(code)),
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// One line of at most `MAX_MESSAGE_CHARS` characters: control characters become spaces,
/// runs of whitespace collapse.
pub(crate) fn one_line(s: &str) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= MAX_MESSAGE_CHARS {
        return one;
    }
    let mut out: String = one.chars().take(MAX_MESSAGE_CHARS - 3).collect();
    out.push_str("...");
    out
}

/// A comment POST that failed in transport: only an error before anything was sent
/// (connect, DNS, building the request) is a definite "not posted".
fn unconfirmed_unless_unsent(e: reqwest::Error) -> ProviderError {
    if e.is_connect() || e.is_builder() {
        network_error(e)
    } else {
        ProviderError::Unconfirmed
    }
}

/// Fixed `Network` message for a TLS certificate the client does not trust.
pub const CERT_UNTRUSTED: &str = "certificate not trusted";

/// True when a failed connect was a certificate problem (walks the error chain; the text
/// is only inspected, never forwarded).
fn is_cert_error(e: &reqwest::Error) -> bool {
    use std::error::Error;
    let mut cur = e.source();
    while let Some(err) = cur {
        if looks_like_cert_error(&err.to_string()) {
            return true;
        }
        cur = err.source();
    }
    false
}

fn looks_like_cert_error(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("invalid peer certificate")
        || t.contains("invalidcertificate")
        || t.contains("unknownissuer")
        || t.contains("unknown issuer")
        || t.contains("certificate verify failed")
        || t.contains("self signed certificate")
        || t.contains("self-signed certificate")
        || t.contains("certificate expired")
}

/// Fixed messages only: reqwest errors are never forwarded verbatim.
fn network_error(e: reqwest::Error) -> ProviderError {
    let msg = if e.is_connect() && is_cert_error(&e) {
        CERT_UNTRUSTED
    } else if e.is_timeout() {
        "request timed out"
    } else if e.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    ProviderError::Network(msg.into())
}

fn decode_message(e: &reqwest::Error) -> String {
    if e.is_decode() {
        "invalid JSON in response body".into()
    } else {
        "failed to read response body".into()
    }
}

#[cfg(test)]
mod tests;
