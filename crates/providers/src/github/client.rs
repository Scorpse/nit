//! GitHub's HTTP rules on top of the shared [`ApiClient`] (spec 4.4, 8.1):
//! - `Authorization: Bearer`, `Accept: application/vnd.github+json` and the pinned
//!   `X-GitHub-Api-Version` on every request;
//! - rate limits: a 403 or 429 is a rate limit only with `retry-after` or
//!   `x-ratelimit-remaining: 0`; a short wait is taken once, a longer one pauses the client
//!   until the reset and fails fast with `RateLimited` (no request) until then;
//! - a 403 with `X-GitHub-SSO` is the SSO hint (the authorization URL is kept for the UI only);
//! - `link` header paging, followed on the API origin only; conditional GET with an ETag;
//! - GraphQL `errors` mapped to fixed errors (never the server's text, which names repos);
//! - writes (REST and GraphQL mutations) spaced by `MIN_WRITE_SPACING`, no redirects, never
//!   retried; a lost answer is `Unconfirmed`, a 4xx refusal `Rejected` with GitHub's message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderName, HeaderValue, ACCEPT, IF_NONE_MATCH};
use reqwest::Method;
use serde_json::{json, Value};

use crate::http::{one_line, ApiClient, AuthScheme, ClientOpts, RawResponse};
use crate::provider::ProviderError;

/// REST API version sent with every request ([API versions]); moving to a newer one is a
/// separate task with re-recorded fixtures.
///
/// [API versions]: https://docs.github.com/en/rest/about-the-rest-api/api-versions
pub const API_VERSION: &str = "2022-11-28";

/// A rate-limit wait up to this long is taken in place (once); a longer one pauses.
pub const MAX_INLINE_WAIT: Duration = Duration::from_secs(10);
/// Pause when GitHub says "rate limited" without saying until when (docs: at least a minute).
pub const DEFAULT_PAUSE: Duration = Duration::from_secs(60);
/// Loop guard for `link` paging.
pub const MAX_PAGES: usize = 100;
/// Least time between the starts of two mutating requests of one client ([best practices]:
/// at least one second between writes, spec 9).
///
/// [best practices]: https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api
pub const MIN_WRITE_SPACING: Duration = Duration::from_secs(1);

/// What a 403 with `X-GitHub-SSO` means for the user.
pub const SSO_HINT: &str =
    "authorize this token for the organization (SAML single sign-on), then try again";

/// The answer of a conditional GET.
#[derive(Debug, PartialEq)]
pub enum Conditional {
    /// 304: nothing changed since the ETag was issued.
    NotModified,
    Fresh {
        body: Value,
        etag: Option<String>,
    },
}

/// One GitHub account's API client (REST and GraphQL on one origin).
pub struct GhClient {
    api: ApiClient,
    graphql_url: String,
    paused_until: Mutex<Option<SystemTime>>,
    /// Requests that count against the primary limit (every answer but 304).
    requests: AtomicU64,
    /// The authorization URL of the last SSO refusal. Shown in the UI as a link the user
    /// may open; never logged or written to a report.
    sso_url: Mutex<Option<String>>,
    /// `X-Accepted-GitHub-Permissions` of the last 403 (fine-grained token hint).
    accepted_permissions: Mutex<Option<String>>,
    /// `X-OAuth-Scopes` of the last answer that had it (classic tokens).
    oauth_scopes: Mutex<Option<String>>,
    /// `(remaining, limit)` from the last answer with `x-ratelimit-*` headers (GHES with
    /// rate limits off sends none).
    rate: Mutex<Option<(u64, u64)>>,
    /// The earliest start of the next write (`MIN_WRITE_SPACING` after the last one).
    next_write: Mutex<Option<tokio::time::Instant>>,
}

impl std::fmt::Debug for GhClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhClient")
            .field("api", &self.api)
            .field("graphql_url", &self.graphql_url)
            .finish()
    }
}

enum Class {
    Ok,
    RateLimited(Duration),
    Err(ProviderError),
}

impl GhClient {
    /// `api_base`: `https://api.github.com` or `https://HOST/api/v3`; `graphql_url` must be on
    /// the same origin (`https://api.github.com/graphql`, `https://HOST/api/graphql`).
    pub fn new(api_base: &str, graphql_url: &str, token: &str) -> GhClient {
        let opts = ClientOpts {
            extra_headers: vec![
                (
                    ACCEPT,
                    HeaderValue::from_static("application/vnd.github+json"),
                ),
                (
                    HeaderName::from_static("x-github-api-version"),
                    HeaderValue::from_static(API_VERSION),
                ),
            ],
            retry_429: false,
            ..ClientOpts::default()
        };
        GhClient {
            api: ApiClient::new(api_base, AuthScheme::Bearer(token.to_string()), opts),
            graphql_url: graphql_url.trim_end_matches('/').to_string(),
            paused_until: Mutex::new(None),
            requests: AtomicU64::new(0),
            sso_url: Mutex::new(None),
            accepted_permissions: Mutex::new(None),
            oauth_scopes: Mutex::new(None),
            rate: Mutex::new(None),
            next_write: Mutex::new(None),
        }
    }

    pub fn api_base(&self) -> &str {
        self.api.base_url()
    }

    /// Until when this client refuses to send (rate limited), if it does.
    pub fn paused_until(&self) -> Option<SystemTime> {
        let slot = lock(&self.paused_until);
        slot.filter(|t| *t > SystemTime::now())
    }

    /// Requests sent that count against the primary rate limit (304 answers do not).
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// `(remaining, limit)` of the primary rate limit as GitHub last reported it.
    pub fn rate_limit(&self) -> Option<(u64, u64)> {
        *lock(&self.rate)
    }

    /// The SSO authorization URL of the last SSO refusal (for a link in the UI only).
    pub fn sso_url(&self) -> Option<String> {
        lock(&self.sso_url).clone()
    }

    /// The fine-grained permissions GitHub said the last refused call needed.
    pub fn accepted_permissions(&self) -> Option<String> {
        lock(&self.accepted_permissions).clone()
    }

    /// The classic token scopes GitHub reported (`X-OAuth-Scopes`); None for fine-grained
    /// tokens, which do not send the header.
    pub fn oauth_scopes(&self) -> Option<String> {
        lock(&self.oauth_scopes).clone()
    }

    /// GET a path or same-origin URL; 2xx only.
    pub async fn get(&self, path: &str) -> Result<RawResponse, ProviderError> {
        self.execute(|| self.api.get_raw(path, &[])).await
    }

    pub async fn get_json(&self, path: &str) -> Result<Value, ProviderError> {
        self.get(path).await?.json()
    }

    /// GET with `If-None-Match: etag` when one is given.
    pub async fn get_conditional(
        &self,
        path: &str,
        etag: Option<&str>,
    ) -> Result<Conditional, ProviderError> {
        let headers: Vec<(HeaderName, HeaderValue)> = etag
            .and_then(|e| HeaderValue::from_str(e).ok())
            .map(|v| vec![(IF_NONE_MATCH, v)])
            .unwrap_or_default();
        let r = self.execute(|| self.api.get_raw(path, &headers)).await?;
        if r.status == 304 {
            return Ok(Conditional::NotModified);
        }
        Ok(Conditional::Fresh {
            body: r.json()?,
            etag: r.header("etag").map(str::to_string),
        })
    }

    /// Every element of a paged list (JSON arrays), following `link: rel="next"` on the API
    /// origin only (another origin is an error, never a silent stop).
    pub async fn get_paged(&self, path: &str) -> Result<Vec<Value>, ProviderError> {
        let mut out = Vec::new();
        let mut next = Some(path.to_string());
        let mut pages = 0;
        while let Some(url) = next.take() {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(ProviderError::Decode("too many pages".into()));
            }
            let r = self.get(&url).await?;
            match r.json()? {
                Value::Array(items) => out.extend(items),
                _ => return Err(ProviderError::Decode("expected a list".into())),
            }
            next = r.header("link").and_then(next_link);
        }
        Ok(out)
    }

    /// Runs a GraphQL query and returns its `data`. Any entry in `errors` fails the call:
    /// `NOT_FOUND` -> `NotFound`, `FORBIDDEN` -> `Forbidden`, `RATE_LIMITED` -> paused and
    /// `RateLimited`, anything else `Decode` with the error type only.
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value, ProviderError> {
        let body = json!({ "query": query, "variables": variables });
        let r = self
            .execute(|| self.api.post_raw(&self.graphql_url, &body))
            .await?;
        self.graphql_data(r.json()?)
    }

    /// `data` of a GraphQL answer, its `errors` mapped as `graphql` documents.
    fn graphql_data(&self, mut v: Value) -> Result<Value, ProviderError> {
        if let Some(errors) = v.get("errors").and_then(Value::as_array) {
            if let Some(first) = errors.first() {
                let kind = first.get("type").and_then(Value::as_str).unwrap_or("");
                return Err(match kind {
                    "NOT_FOUND" => ProviderError::NotFound,
                    "FORBIDDEN" => ProviderError::Forbidden,
                    "RATE_LIMITED" => {
                        self.pause(DEFAULT_PAUSE);
                        ProviderError::RateLimited
                    }
                    "" => ProviderError::Decode("graphql error".into()),
                    k => ProviderError::Decode(format!("graphql error {}", safe_word(k))),
                });
            }
        }
        match v.get_mut("data").map(Value::take) {
            Some(d) if !d.is_null() => Ok(d),
            _ => Err(ProviderError::Decode("graphql answer without data".into())),
        }
    }

    /// A mutating REST request (POST/PUT/DELETE) with a JSON body; returns the JSON answer
    /// (`Null` when empty). Spaced by `MIN_WRITE_SPACING`, follows no redirect, never
    /// retried:
    /// - not sent: `Network`; sent but no answer, or a 5xx: `Unconfirmed`;
    /// - a 2xx whose body is not JSON: `PostedUnreadable`;
    /// - rate limited (403/429 with the rate-limit headers, or any 429): `RateLimited`, and
    ///   the client pauses for the wait asked;
    /// - 400/403/404/409/422: `Rejected` with GitHub's message (one line, sanitized) when it
    ///   gives one.
    pub async fn write(
        &self,
        method: Method,
        path: &str,
        body: &Value,
    ) -> Result<Value, ProviderError> {
        if self.paused_until().is_some() {
            return Err(ProviderError::RateLimited);
        }
        self.space_write().await;
        let r = self.api.write_raw(method, path, Some(body)).await?;
        self.note(&r);
        self.classify_write(&r)?;
        if r.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&r.body).map_err(|_| ProviderError::PostedUnreadable)
    }

    /// A GraphQL mutation: sent like `write`, its answer read like `graphql`.
    pub async fn graphql_mutation(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<Value, ProviderError> {
        let body = json!({ "query": query, "variables": variables });
        let v = self.write(Method::POST, &self.graphql_url, &body).await?;
        self.graphql_data(v)
    }

    /// Waits for this write's turn and reserves the next one `MIN_WRITE_SPACING` later
    /// (concurrent writes queue up instead of starting together).
    async fn space_write(&self) {
        let now = tokio::time::Instant::now();
        let start = {
            let mut next = lock(&self.next_write);
            let start = next.map_or(now, |t| t.max(now));
            *next = Some(start + MIN_WRITE_SPACING);
            start
        };
        tokio::time::sleep_until(start).await;
    }

    fn classify_write(&self, r: &RawResponse) -> Result<(), ProviderError> {
        match r.status {
            200..=299 => Ok(()),
            500..=599 => Err(ProviderError::Unconfirmed),
            401 => Err(ProviderError::Auth),
            403 | 429 => match self.classify(r) {
                Class::RateLimited(wait) => {
                    self.pause(if wait.is_zero() { DEFAULT_PAUSE } else { wait });
                    Err(ProviderError::RateLimited)
                }
                Class::Err(ProviderError::Forbidden) => Err(match write_message(r) {
                    Some(message) => ProviderError::Rejected {
                        status: 403,
                        message,
                    },
                    None => ProviderError::Forbidden,
                }),
                Class::Err(e) => Err(e),
                Class::Ok => Ok(()),
            },
            status @ (400 | 404 | 409 | 422) => Err(match (write_message(r), status) {
                (Some(message), _) => ProviderError::Rejected { status, message },
                (None, 404) => ProviderError::NotFound,
                (None, 422) => ProviderError::Rejected {
                    status,
                    message: "the server could not process it".into(),
                },
                (None, code) => ProviderError::Http(code),
            }),
            code => Err(ProviderError::Http(code)),
        }
    }

    /// Counts the request and keeps the rate-limit and scope headers.
    fn note(&self, r: &RawResponse) {
        if r.status != 304 {
            self.requests.fetch_add(1, Ordering::Relaxed);
        }
        let num = |k: &str| r.header(k).and_then(|v| v.trim().parse::<u64>().ok());
        if let (Some(rem), Some(lim)) = (num("x-ratelimit-remaining"), num("x-ratelimit-limit")) {
            *lock(&self.rate) = Some((rem, lim));
        }
        if let Some(s) = r.header("x-oauth-scopes") {
            *lock(&self.oauth_scopes) = Some(s.to_string());
        }
    }

    /// Sends (at most twice: once more after a short rate-limit wait) and classifies.
    async fn execute<F, Fut>(&self, send: F) -> Result<RawResponse, ProviderError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<RawResponse, ProviderError>>,
    {
        let mut waited = false;
        loop {
            if self.paused_until().is_some() {
                return Err(ProviderError::RateLimited);
            }
            let r = send().await?;
            self.note(&r);
            match self.classify(&r) {
                Class::Ok => return Ok(r),
                Class::Err(e) => return Err(e),
                Class::RateLimited(wait) if !waited && wait <= MAX_INLINE_WAIT => {
                    waited = true;
                    tokio::time::sleep(wait).await;
                }
                Class::RateLimited(wait) => {
                    self.pause(if wait.is_zero() { DEFAULT_PAUSE } else { wait });
                    return Err(ProviderError::RateLimited);
                }
            }
        }
    }

    fn pause(&self, d: Duration) {
        *lock(&self.paused_until) = Some(SystemTime::now() + d);
    }

    fn classify(&self, r: &RawResponse) -> Class {
        match r.status {
            200..=299 | 304 => Class::Ok,
            401 => Class::Err(ProviderError::Auth),
            403 | 429 => {
                if let Some(wait) = rate_limit_wait(r) {
                    return Class::RateLimited(wait);
                }
                if r.status == 429 {
                    return Class::RateLimited(DEFAULT_PAUSE);
                }
                if let Some(sso) = r.header("x-github-sso") {
                    *lock(&self.sso_url) = sso_url(sso);
                    return Class::Err(ProviderError::Rejected {
                        status: 403,
                        message: SSO_HINT.into(),
                    });
                }
                if let Some(p) = r.header("x-accepted-github-permissions") {
                    *lock(&self.accepted_permissions) = Some(p.to_string());
                }
                Class::Err(ProviderError::Forbidden)
            }
            404 => Class::Err(ProviderError::NotFound),
            code => Class::Err(ProviderError::Http(code)),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The wait a rate-limited answer asks for: `retry-after` seconds, else the time until
/// `x-ratelimit-reset` when `x-ratelimit-remaining` is 0. None: not a rate limit.
fn rate_limit_wait(r: &RawResponse) -> Option<Duration> {
    if let Some(secs) = r
        .header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_secs(secs));
    }
    if r.header("x-ratelimit-remaining").map(str::trim) == Some("0") {
        let reset = r
            .header("x-ratelimit-reset")
            .and_then(|v| v.trim().parse::<u64>().ok())?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        return Some(Duration::from_secs(reset.saturating_sub(now)));
    }
    None
}

/// GitHub's refusal text: `message`, plus the first entry of `errors` (a string, or an
/// object's `message`) when there is one, as one sanitized line. Only these two fields are
/// read.
fn write_message(r: &RawResponse) -> Option<String> {
    let top = r.message()?;
    let v: Value = serde_json::from_slice(&r.body).ok()?;
    let detail = v["errors"]
        .as_array()
        .and_then(|e| e.first())
        .and_then(|e| e.as_str().or_else(|| e["message"].as_str()))
        .map(one_line)
        .filter(|d| !d.is_empty());
    Some(match detail {
        Some(d) => one_line(&format!("{top}: {d}")),
        None => top,
    })
}

/// The `url=` part of `X-GitHub-SSO: required; url=https://github.com/orgs/...`, only when it
/// is an https URL (it is opened only on a user click, and only on the account's host or
/// github.com: the UI checks that).
fn sso_url(header: &str) -> Option<String> {
    header
        .split(';')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("url="))
        .filter(|u| u.starts_with("https://"))
        .map(str::to_string)
}

/// The `rel="next"` target of a `link` header.
pub fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let mut pieces = part.split(';');
        let url = pieces.next()?.trim();
        let is_next = pieces.any(|p| {
            let p = p.trim();
            p == "rel=\"next\"" || p == "rel=next"
        });
        (is_next && url.starts_with('<') && url.ends_with('>'))
            .then(|| url[1..url.len() - 1].to_string())
    })
}

/// Letters, digits and `_` only: a GraphQL error type is shown, its message never.
fn safe_word(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(40)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_link_finds_rel_next_only() {
        let h = r#"<https://api.github.com/user/repos?page=2>; rel="next", <https://api.github.com/user/repos?page=5>; rel="last""#;
        assert_eq!(
            next_link(h).as_deref(),
            Some("https://api.github.com/user/repos?page=2")
        );
        assert_eq!(
            next_link(r#"<https://x/?page=1>; rel="prev", <https://x/?page=1>; rel="first""#),
            None
        );
        assert_eq!(next_link(""), None);
    }

    #[test]
    fn sso_url_needs_https() {
        assert_eq!(
            sso_url("required; url=https://github.com/orgs/o/sso?authorization_request=A")
                .as_deref(),
            Some("https://github.com/orgs/o/sso?authorization_request=A")
        );
        assert_eq!(sso_url("required; url=javascript:alert(1)"), None);
        assert_eq!(sso_url("partial-results; organizations=21955855"), None);
    }

    #[test]
    fn debug_never_contains_token() {
        let c = GhClient::new(
            "https://api.github.com",
            "https://api.github.com/graphql",
            "gh-s3cret-T0KEN",
        );
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("gh-s3cret-T0KEN") && shown.contains("***"),
            "{shown}"
        );
    }
}
