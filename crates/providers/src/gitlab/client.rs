//! GitLab's HTTP rules on top of the shared [`ApiClient`] (spec 4.4, 8.2):
//! - `PRIVATE-TOKEN` on every request ([REST auth]), to the API base only (a relative URL
//!   root such as `https://host/gitlab/api/v4` included);
//! - rate limits: a 429 waits `Retry-After` once when it is short, else the client pauses
//!   until then and fails fast with `RateLimited` (no request) until it is over;
//!   `RateLimit-Remaining` / `RateLimit-Limit` are kept for the token check;
//! - offset paging: `Link: rel="next"` on the API origin only, else `x-next-page`;
//! - writes (REST and GraphQL mutations) spaced by `MIN_WRITE_SPACING`, no redirects, never
//!   retried; a lost answer is `Unconfirmed`, a 4xx refusal `Rejected` with GitLab's message,
//!   a 429 `RateLimited` (the client pauses for the wait asked).
//!
//! [REST auth]: https://docs.gitlab.com/api/rest/authentication/

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use reqwest::Method;
use serde_json::{json, Value};

use crate::http::{one_line, ApiClient, AuthScheme, ClientOpts, RawResponse};
use crate::provider::ProviderError;

/// Least time between the starts of two mutating requests of one client (spec 7.5: writes
/// are sequential; gitlab.com allows about one note a second).
pub const MIN_WRITE_SPACING: Duration = Duration::from_secs(1);

/// A `Retry-After` up to this long is waited in place (once); a longer one pauses.
pub const MAX_INLINE_WAIT: Duration = Duration::from_secs(10);
/// Pause after a 429 that says nothing about how long.
pub const DEFAULT_PAUSE: Duration = Duration::from_secs(60);
/// Loop guard for paging.
pub const MAX_PAGES: usize = 100;

/// One GitLab account's API client.
pub struct GlClient {
    api: ApiClient,
    /// `<web>/api/graphql`, next to the REST base `<web>/api/v4`.
    graphql_url: String,
    /// The earliest start of the next write (`MIN_WRITE_SPACING` after the last one).
    next_write: Mutex<Option<tokio::time::Instant>>,
    paused_until: Mutex<Option<SystemTime>>,
    requests: AtomicU64,
    /// `(remaining, limit)` from the last answer with `RateLimit-*` headers.
    rate: Mutex<Option<(u64, u64)>>,
}

impl std::fmt::Debug for GlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlClient").field("api", &self.api).finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

enum Class {
    Ok,
    RateLimited(Duration),
    Err(ProviderError),
}

impl GlClient {
    /// `api_base`: `https://gitlab.com/api/v4`, `https://host/api/v4` or
    /// `https://host/prefix/api/v4`.
    pub fn new(api_base: &str, token: &str) -> GlClient {
        let opts = ClientOpts {
            retry_429: false,
            ..ClientOpts::default()
        };
        let base = api_base.trim_end_matches('/');
        let graphql_url = format!("{}/graphql", base.strip_suffix("/v4").unwrap_or(base));
        GlClient {
            api: ApiClient::new(api_base, AuthScheme::PrivateToken(token.to_string()), opts),
            graphql_url,
            next_write: Mutex::new(None),
            paused_until: Mutex::new(None),
            requests: AtomicU64::new(0),
            rate: Mutex::new(None),
        }
    }

    pub fn api_base(&self) -> &str {
        self.api.base_url()
    }

    /// Until when this client refuses to send (rate limited), if it does.
    pub fn paused_until(&self) -> Option<SystemTime> {
        lock(&self.paused_until).filter(|t| *t > SystemTime::now())
    }

    /// Requests sent by this client.
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// `(remaining, limit)` as GitLab last reported them (self-managed may send none).
    pub fn rate_limit(&self) -> Option<(u64, u64)> {
        *lock(&self.rate)
    }

    /// GET a path (under the API base) or a same-origin URL; 2xx only.
    pub async fn get(&self, path: &str) -> Result<RawResponse, ProviderError> {
        let mut waited = false;
        loop {
            if self.paused_until().is_some() {
                return Err(ProviderError::RateLimited);
            }
            let r = self.api.get_raw(path, &[]).await?;
            self.note(&r);
            match classify(&r) {
                Class::Ok => return Ok(r),
                Class::Err(e) => return Err(e),
                Class::RateLimited(wait) if !waited && wait <= MAX_INLINE_WAIT => {
                    waited = true;
                    tokio::time::sleep(wait).await;
                }
                Class::RateLimited(wait) => {
                    *lock(&self.paused_until) = Some(SystemTime::now() + wait);
                    return Err(ProviderError::RateLimited);
                }
            }
        }
    }

    pub async fn get_json(&self, path: &str) -> Result<Value, ProviderError> {
        self.get(path).await?.json()
    }

    /// Every element of a paged list (JSON arrays): `Link: rel="next"` when it is on the
    /// client's own origin, else `x-next-page` on the URL just read; an empty or missing
    /// `x-next-page` ends the list. A next link on another origin is never followed: GitLab
    /// builds it from its `external_url`, which can differ from the address nit uses
    /// (live GitLab 19.5.1 at `http://IP:8080` linked to `https://IP:8443`), so the page
    /// number comes from `x-next-page`; without one it is an error, never a silent stop.
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
            let x_next = r
                .header("x-next-page")
                .and_then(|p| p.trim().parse::<u32>().ok())
                .filter(|n| *n > 0)
                .map(|n| with_page(&url, n));
            next = match r.header("link").and_then(crate::github::next_link) {
                Some(link) if self.api.is_same_origin(&link) => Some(link),
                // the foreign link itself goes to `get`, which refuses it (an error)
                Some(link) => x_next.or(Some(link)),
                None => x_next,
            };
        }
        Ok(out)
    }

    /// The GraphQL endpoint (`<web>/api/graphql`).
    pub fn graphql_url(&self) -> &str {
        &self.graphql_url
    }

    /// A mutating REST request (POST/PUT/DELETE), with a JSON body when given; returns the
    /// JSON answer (`Null` when empty). Spaced by `MIN_WRITE_SPACING`, follows no redirect,
    /// never retried:
    /// - not sent: `Network`; sent but no answer, or a 5xx: `Unconfirmed`;
    /// - a 2xx whose body is not JSON: `PostedUnreadable`;
    /// - 429: `RateLimited`, and the client pauses for `Retry-After` (or `DEFAULT_PAUSE`);
    /// - 400/403/404/409/422: `Rejected` with GitLab's message (one line, sanitized) when it
    ///   gives one (403 and 404 without one are `Forbidden` / `NotFound`).
    pub async fn write(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ProviderError> {
        if self.paused_until().is_some() {
            return Err(ProviderError::RateLimited);
        }
        self.space_write().await;
        let r = self.api.write_raw(method, path, body).await?;
        self.note(&r);
        self.classify_write(&r)?;
        if r.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&r.body).map_err(|_| ProviderError::PostedUnreadable)
    }

    /// A GraphQL mutation, sent like `write`. Returns `data.<name>`; a top-level `errors` entry
    /// or a non-empty `data.<name>.errors` (GitLab's mutation errors) is `Rejected`.
    pub async fn graphql_mutation(
        &self,
        name: &str,
        query: &str,
        variables: Value,
    ) -> Result<Value, ProviderError> {
        let body = json!({ "query": query, "variables": variables });
        let v = self
            .write(Method::POST, &self.graphql_url, Some(&body))
            .await?;
        let rejected = |m: Option<&str>| ProviderError::Rejected {
            status: 422,
            message: m
                .map(one_line)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "GitLab refused the request".into()),
        };
        if let Some(first) = v["errors"].as_array().and_then(|e| e.first()) {
            return Err(rejected(first["message"].as_str()));
        }
        let out = v["data"][name].clone();
        if out.is_null() {
            return Err(ProviderError::Decode("graphql answer without data".into()));
        }
        if let Some(first) = out["errors"].as_array().and_then(|e| e.first()) {
            return Err(rejected(first.as_str()));
        }
        Ok(out)
    }

    /// Waits for this write's turn and reserves the next one `MIN_WRITE_SPACING` later.
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
            429 => {
                let wait = r
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map_or(DEFAULT_PAUSE, Duration::from_secs);
                *lock(&self.paused_until) = Some(SystemTime::now() + wait);
                Err(ProviderError::RateLimited)
            }
            status @ (400 | 403 | 404 | 409 | 422) => Err(match (write_message(r), status) {
                (Some(message), _) => ProviderError::Rejected { status, message },
                (None, 403) => ProviderError::Forbidden,
                (None, 404) => ProviderError::NotFound,
                (None, s) => ProviderError::Rejected {
                    status: s,
                    message: "GitLab could not process it".into(),
                },
            }),
            code => Err(ProviderError::Http(code)),
        }
    }

    fn note(&self, r: &RawResponse) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let num = |k: &str| r.header(k).and_then(|v| v.trim().parse::<u64>().ok());
        if let (Some(rem), Some(lim)) = (num("ratelimit-remaining"), num("ratelimit-limit")) {
            *lock(&self.rate) = Some((rem, lim));
        }
    }
}

fn classify(r: &RawResponse) -> Class {
    match r.status {
        200..=299 => Class::Ok,
        401 => Class::Err(ProviderError::Auth),
        403 => Class::Err(ProviderError::Forbidden),
        404 => Class::Err(ProviderError::NotFound),
        429 => Class::RateLimited(
            r.header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(DEFAULT_PAUSE, Duration::from_secs),
        ),
        code => Class::Err(ProviderError::Http(code)),
    }
}

/// GitLab's refusal text, one sanitized line: `message` as a string, or the first entry of a
/// `message` object (`{"base": ["..."]}`, `{"note": ["..."]}`), else `error`. Bodies over
/// 64 KiB are not read.
fn write_message(r: &RawResponse) -> Option<String> {
    if r.body.len() > 64 * 1024 {
        return None;
    }
    let v: Value = serde_json::from_slice(&r.body).ok()?;
    let text = match &v["message"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.iter().next().map(|(k, val)| {
            let first = match val {
                Value::Array(a) => a.first().and_then(Value::as_str).unwrap_or("").to_string(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{k} {first}")
        }),
        _ => v["error"].as_str().map(str::to_string),
    }?;
    Some(one_line(&text)).filter(|m| !m.is_empty())
}

/// `url` with its `page` query parameter set to `n` (added when absent).
fn with_page(url: &str, n: u32) -> String {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let mut parts: Vec<String> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("page="))
        .map(str::to_string)
        .collect();
    parts.push(format!("page={n}"));
    format!("{base}?{}", parts.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_page_replaces_or_adds_the_page() {
        assert_eq!(with_page("/projects?a=1&page=2", 3), "/projects?a=1&page=3");
        assert_eq!(with_page("/projects", 2), "/projects?page=2");
        assert_eq!(
            with_page("https://h/api/v4/x?per_page=100", 4),
            "https://h/api/v4/x?per_page=100&page=4"
        );
    }

    #[test]
    fn debug_never_contains_token() {
        let c = GlClient::new("https://gitlab.com/api/v4", "glpat-s3cret-T0KEN");
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("glpat-s3cret-T0KEN") && shown.contains("***"),
            "{shown}"
        );
    }
}
