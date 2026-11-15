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
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "s3cret-token-xyz";
    const EMAIL: &str = "me@example.com";

    fn client(server: &MockServer) -> BbClient {
        BbClient::new(&server.uri(), EMAIL, TOKEN)
    }

    #[tokio::test]
    async fn get_all_stops_on_a_looping_next_link() {
        let server = MockServer::start().await;
        let me = format!("{}/loop", server.uri());
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(page(json!([1]), Some(me)))
            .mount(&server)
            .await;
        let err = client(&server).get_all("/loop").await.unwrap_err();
        assert_eq!(err, ProviderError::Decode("paging loop".into()));
    }

    #[tokio::test]
    async fn get_all_stops_after_too_many_pages() {
        let server = MockServer::start().await;
        let base = server.uri();
        // every page links to a fresh URL, never ending
        Mock::given(method("GET"))
            .respond_with(move |req: &wiremock::Request| {
                let n: usize = req
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "n")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                ResponseTemplate::new(200).set_body_json(json!({
                    "values": [n],
                    "next": format!("{base}/many?n={}", n + 1)
                }))
            })
            .mount(&server)
            .await;
        let err = client(&server).get_all("/many").await.unwrap_err();
        assert_eq!(err, ProviderError::Decode("paging loop".into()));
    }

    /// A connect that never completes (non-routable address) is a definite "not posted".
    /// Ignored by default: some networks answer or reject the address at once.
    #[tokio::test]
    #[ignore = "needs a network that blackholes 10.255.255.1"]
    async fn connect_timeout_is_definite_not_unconfirmed() {
        let c = BbClient::with_timeouts(
            "http://10.255.255.1:81",
            EMAIL,
            TOKEN,
            Duration::from_secs(5),
            Duration::from_millis(300),
        );
        let started = std::time::Instant::now();
        let err = c.post_create("/x", &json!({"a": 1})).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(err, ProviderError::Network("request timed out".into()));
    }

    fn page(values: serde_json::Value, next: Option<String>) -> ResponseTemplate {
        let mut body = json!({ "values": values });
        if let Some(n) = next {
            body["next"] = json!(n);
        }
        ResponseTemplate::new(200).set_body_json(body)
    }

    #[tokio::test]
    async fn get_all_follows_next_links_and_concatenates() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/items"))
            .and(query_param("pagelen", "100"))
            .respond_with(page(json!([1, 2]), Some(format!("{}/p2?page=2", s.uri()))))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/p2"))
            .respond_with(page(json!([3]), Some(format!("{}/p3?page=3", s.uri()))))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/p3"))
            .respond_with(page(json!([4, 5]), None))
            .mount(&s)
            .await;
        let got = client(&s).get_all("/items").await.unwrap();
        assert_eq!(got, vec![json!(1), json!(2), json!(3), json!(4), json!(5)]);
    }

    #[tokio::test]
    async fn get_all_handles_missing_next() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(page(json!(["a"]), None))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(
            client(&s).get_all("/items").await.unwrap(),
            vec![json!("a")]
        );
    }

    #[tokio::test]
    async fn get_all_empty_values() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(page(json!([]), None))
            .mount(&s)
            .await;
        assert!(client(&s).get_all("/items").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_all_keeps_existing_pagelen_and_appends_with_ampersand() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/items"))
            .and(query_param("state", "OPEN"))
            .and(query_param("pagelen", "10"))
            .respond_with(page(json!([1]), None))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/other"))
            .and(query_param("state", "OPEN"))
            .and(query_param("pagelen", "100"))
            .respond_with(page(json!([2]), None))
            .expect(1)
            .mount(&s)
            .await;
        let c = client(&s);
        assert_eq!(
            c.get_all("/items?state=OPEN&pagelen=10")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(c.get_all("/other?state=OPEN").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rate_limit_then_success_retries() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&s)
            .await;
        let v = client(&s).get_json("/x").await.unwrap();
        assert_eq!(v["ok"], json!(true));
    }

    #[tokio::test]
    async fn rate_limit_exhausted_returns_rate_limited() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .expect(5)
            .mount(&s)
            .await;
        let err = client(&s).get_json("/x").await.unwrap_err();
        assert_eq!(err, ProviderError::RateLimited);
    }

    #[tokio::test]
    async fn rate_limit_mid_pagination_does_not_duplicate_pages() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(page(json!([1, 2]), Some(format!("{}/p2", s.uri()))))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/p2"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/p2"))
            .respond_with(page(json!([3]), None))
            .mount(&s)
            .await;
        let got = client(&s).get_all("/items").await.unwrap();
        assert_eq!(got, vec![json!(1), json!(2), json!(3)]);
    }

    #[tokio::test]
    async fn status_401_maps_to_auth() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        assert_eq!(
            client(&s).get_json("/x").await.unwrap_err(),
            ProviderError::Auth
        );
    }

    #[tokio::test]
    async fn status_403_maps_to_forbidden() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&s)
            .await;
        assert_eq!(
            client(&s).get_json("/x").await.unwrap_err(),
            ProviderError::Forbidden
        );
    }

    #[tokio::test]
    async fn status_404_maps_to_not_found() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        assert_eq!(
            client(&s).get_json("/x").await.unwrap_err(),
            ProviderError::NotFound
        );
    }

    #[tokio::test]
    async fn other_status_maps_to_http_and_bad_json_to_decode() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/boom"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/bad"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&s)
            .await;
        let c = client(&s);
        assert_eq!(
            c.get_json("/boom").await.unwrap_err(),
            ProviderError::Http(500)
        );
        assert!(matches!(
            c.get_json("/bad").await.unwrap_err(),
            ProviderError::Decode(_)
        ));
    }

    #[tokio::test]
    async fn sends_basic_auth_header() {
        let s = MockServer::start().await;
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{EMAIL}:{TOKEN}"))
        );
        Mock::given(method("GET"))
            .and(header("authorization", expected.as_str()))
            .and(header("user-agent", "nit/0.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&s)
            .await;
        client(&s).get_json("/x").await.unwrap();
    }

    #[tokio::test]
    async fn refuses_to_send_credentials_to_another_origin() {
        let s = MockServer::start().await;
        let other = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(0)
            .mount(&other)
            .await;
        let err = client(&s)
            .get_json(&format!("{}/steal", other.uri()))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Network(_)));
    }

    #[tokio::test]
    async fn same_origin_redirect_is_followed() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pr/1/diffstat"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/repo/diffstat"))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/repo/diffstat"))
            .and(header("authorization", client_auth().as_str()))
            .respond_with(page(json!([7]), None))
            .expect(1)
            .mount(&s)
            .await;
        let got = client(&s).get_all("/pr/1/diffstat").await.unwrap();
        assert_eq!(got, vec![json!(7)]);
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_not_followed() {
        let s = MockServer::start().await;
        let other = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(0)
            .mount(&other)
            .await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/steal", other.uri()).as_str()),
            )
            .mount(&s)
            .await;
        assert_eq!(
            client(&s).get_json("/x").await.unwrap_err(),
            ProviderError::Http(302)
        );
    }

    #[tokio::test]
    async fn redirect_loop_stops_at_hop_limit() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/loop"))
            .mount(&s)
            .await;
        assert_eq!(
            client(&s).get_json("/loop").await.unwrap_err(),
            ProviderError::Http(302)
        );
    }

    #[tokio::test]
    async fn get_page_returns_values_and_next_without_following() {
        let s = MockServer::start().await;
        let next = format!("{}/p2", s.uri());
        Mock::given(method("GET"))
            .and(path("/items"))
            .and(query_param("pagelen", "5"))
            .respond_with(page(json!([1, 2]), Some(next.clone())))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/p2"))
            .respond_with(page(json!([3]), None))
            .expect(0)
            .mount(&s)
            .await;
        let (values, got_next) = client(&s).get_page("/items?pagelen=5").await.unwrap();
        assert_eq!(values, vec![json!(1), json!(2)]);
        assert_eq!(got_next, Some(next));
    }

    #[tokio::test]
    async fn get_page_last_page_has_no_next() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(page(json!([9]), None))
            .mount(&s)
            .await;
        let (values, next) = client(&s).get_page("/items").await.unwrap();
        assert_eq!(values, vec![json!(9)]);
        assert_eq!(next, None);
    }

    fn client_auth() -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{EMAIL}:{TOKEN}"))
        )
    }

    #[test]
    fn debug_output_never_contains_token() {
        let c = BbClient::new("http://localhost:1", EMAIL, TOKEN);
        let dbg = format!("{c:?}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{EMAIL}:{TOKEN}"));
        assert!(!dbg.contains(TOKEN));
        assert!(!dbg.contains(&b64));
        assert!(dbg.contains("token: \"***\""));
    }

    #[tokio::test]
    async fn error_display_never_contains_token() {
        let mut errs = vec![
            ProviderError::Auth,
            ProviderError::Forbidden,
            ProviderError::NotFound,
            ProviderError::RateLimited,
            ProviderError::Http(500),
            ProviderError::Decode("x".into()),
        ];
        // A real connection failure (nothing listens on port 1).
        errs.push(
            BbClient::new("http://127.0.0.1:1", EMAIL, TOKEN)
                .get_json("/x")
                .await
                .unwrap_err(),
        );
        for e in errs {
            let shown = format!("{e} | {e:?}");
            assert!(!shown.contains(TOKEN), "leaked in: {shown}");
        }
    }

    #[tokio::test]
    async fn post_json_sends_body_and_basic_auth_and_returns_status_and_body() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/c"))
            .and(header("authorization", client_auth().as_str()))
            .and(header("user-agent", "nit/0.1"))
            .and(wiremock::matchers::body_json(json!({"a": 1})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 9})))
            .expect(1)
            .mount(&s)
            .await;
        let (status, body) = client(&s).post_json("/c", &json!({"a": 1})).await.unwrap();
        assert_eq!(status, 201);
        assert_eq!(body, json!({"id": 9}));
    }

    #[tokio::test]
    async fn post_json_returns_error_statuses_with_their_body() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bad"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "nope"})))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/text"))
            .respond_with(ResponseTemplate::new(500).set_body_string("plain failure"))
            .mount(&s)
            .await;
        let c = client(&s);
        let (st, body) = c.post_json("/bad", &json!({})).await.unwrap();
        assert_eq!((st, body), (400, json!({"error": "nope"})));
        let (st, body) = c.post_json("/text", &json!({})).await.unwrap();
        assert_eq!((st, body), (500, json!("plain failure")));
    }

    #[tokio::test]
    async fn post_json_retries_429_and_refuses_other_origins() {
        let s = MockServer::start().await;
        let other = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"ok": true})))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&other)
            .await;
        let c = client(&s);
        assert_eq!(c.post_json("/x", &json!({})).await.unwrap().0, 201);
        let err = c
            .post_json(&format!("{}/steal", other.uri()), &json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Network(_)));
    }

    #[tokio::test]
    async fn post_json_rate_limit_exhausted_and_errors_never_leak_token() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .expect(5)
            .mount(&s)
            .await;
        let e1 = client(&s).post_json("/x", &json!({})).await.unwrap_err();
        assert_eq!(e1, ProviderError::RateLimited);
        let e2 = BbClient::new("http://127.0.0.1:1", EMAIL, TOKEN)
            .post_json("/x", &json!({}))
            .await
            .unwrap_err();
        for e in [e1, e2] {
            let shown = format!("{e} | {e:?}");
            assert!(!shown.contains(TOKEN), "leaked in: {shown}");
        }
    }

    #[tokio::test]
    async fn get_text_returns_raw_body_and_maps_statuses() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/diff"))
            .and(header("authorization", client_auth().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "diff --git a/x b/x
",
            ))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        let c = client(&s);
        assert_eq!(
            c.get_text("/diff").await.unwrap(),
            "diff --git a/x b/x
"
        );
        assert_eq!(
            c.get_text("/gone").await.unwrap_err(),
            ProviderError::NotFound
        );
    }
}
