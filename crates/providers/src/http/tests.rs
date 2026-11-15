use super::*;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "s3cret-token-xyz";

fn opts() -> ClientOpts {
    ClientOpts::default()
}

#[test]
fn certificate_messages_are_recognised() {
    assert!(looks_like_cert_error(
        "invalid peer certificate: UnknownIssuer"
    ));
    assert!(looks_like_cert_error("invalid peer certificate: Expired"));
    assert!(!looks_like_cert_error("connection refused"));
}

#[tokio::test]
async fn bearer_and_private_token_headers_are_sensitive() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/p"))
        .and(header("private-token", TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&s)
        .await;
    let bearer = AuthScheme::Bearer(TOKEN.into());
    let private = AuthScheme::PrivateToken(TOKEN.into());
    assert!(bearer.header().1.is_sensitive());
    assert!(private.header().1.is_sensitive());
    ApiClient::new(&s.uri(), bearer, opts())
        .get_json("/b")
        .await
        .unwrap();
    ApiClient::new(&s.uri(), private, opts())
        .get_json("/p")
        .await
        .unwrap();
}

#[test]
fn debug_never_contains_token() {
    let b64 = base64::engine::general_purpose::STANDARD.encode(format!("me:{TOKEN}"));
    for auth in [
        AuthScheme::Basic {
            user: "me".into(),
            token: TOKEN.into(),
        },
        AuthScheme::Bearer(TOKEN.into()),
        AuthScheme::PrivateToken(TOKEN.into()),
    ] {
        let shown = format!(
            "{auth:?} {:?}",
            ApiClient::new("https://h.example", auth.clone(), opts())
        );
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(!shown.contains(&b64), "{shown}");
        assert!(shown.contains("***"), "{shown}");
    }
}

#[test]
fn clients_share_one_connection_pool_unless_connect_timeout_differs() {
    let a = ApiClient::new("https://a.example", AuthScheme::Bearer("x".into()), opts());
    let b = ApiClient::new("https://b.example", AuthScheme::Bearer("y".into()), opts());
    assert!(a.uses_shared_pool() && b.uses_shared_pool());
    let own = ApiClient::new(
        "https://c.example",
        AuthScheme::Bearer("z".into()),
        ClientOpts {
            connect_timeout: Duration::from_millis(5),
            ..opts()
        },
    );
    assert!(!own.uses_shared_pool());
}

/// The pool is shared, so the redirect rule must use each request's own origin: a
/// redirect from client A's server to client B's server is not followed.
#[tokio::test]
async fn shared_pool_still_refuses_cross_origin_redirects() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"b": 1})))
        .mount(&b)
        .await;
    Mock::given(method("GET"))
        .and(path("/steal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(0)
        .mount(&b)
        .await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/steal", b.uri()).as_str()),
        )
        .mount(&a)
        .await;
    let ca = ApiClient::new(&a.uri(), AuthScheme::Bearer(TOKEN.into()), opts());
    let cb = ApiClient::new(&b.uri(), AuthScheme::Bearer("other".into()), opts());
    assert_eq!(cb.get_json("/ok").await.unwrap(), json!({"b": 1}));
    assert_eq!(
        ca.get_json("/x").await.unwrap_err(),
        ProviderError::Http(302)
    );
}

/// A plain-http account (private address): a redirect to https on the same host and
/// port is another origin and is not followed (had it been, the TLS hello to the plain
/// server would fail as a network error instead of the 302).
#[tokio::test]
async fn http_to_https_redirect_is_not_followed() {
    let s = MockServer::start().await;
    let port = s.address().port();
    Mock::given(method("GET"))
        .and(path("/x"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("https://127.0.0.1:{port}/x").as_str()),
        )
        .expect(1)
        .mount(&s)
        .await;
    let c = ApiClient::new(
        &format!("http://127.0.0.1:{port}"),
        AuthScheme::PrivateToken(TOKEN.into()),
        opts(),
    );
    assert_eq!(
        c.get_json("/x").await.unwrap_err(),
        ProviderError::Http(302)
    );
    // nor an absolute https URL on the same host when the base is http
    assert!(matches!(
        c.get_raw(&format!("https://127.0.0.1:{port}/x"), &[]).await,
        Err(ProviderError::Network(m)) if m.contains("different origin")
    ));
    let http = Url::parse("http://192.168.1.5:8080/a").unwrap();
    for other in [
        "https://192.168.1.5:8080/a",
        "http://192.168.1.5:8081/a",
        "http://192.168.1.6:8080/a",
    ] {
        assert!(!same_origin(&http, &Url::parse(other).unwrap()), "{other}");
    }
    assert!(same_origin(
        &http,
        &Url::parse("http://192.168.1.5:8080/b").unwrap()
    ));
}

#[tokio::test]
async fn only_429_is_retried() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&s)
        .await;
    let c = ApiClient::new(&s.uri(), AuthScheme::Bearer(TOKEN.into()), opts());
    assert_eq!(
        c.get_json("/x").await.unwrap_err(),
        ProviderError::Http(503)
    );
}

#[tokio::test]
async fn extra_headers_are_sent() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("x-api-version", "2022-11-28"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&s)
        .await;
    let c = ApiClient::new(
        &s.uri(),
        AuthScheme::Bearer(TOKEN.into()),
        ClientOpts {
            extra_headers: vec![(
                HeaderName::from_static("x-api-version"),
                HeaderValue::from_static("2022-11-28"),
            )],
            ..opts()
        },
    );
    c.get_json("/x").await.unwrap();
}

#[tokio::test]
async fn error_shape_supplies_the_rejection_message() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gh"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({"message": "Validation\n  Failed"})),
        )
        .mount(&s)
        .await;
    let c = ApiClient::new(&s.uri(), AuthScheme::Bearer(TOKEN.into()), opts());
    assert_eq!(
        c.post_checked("/gh", None).await.unwrap_err(),
        ProviderError::Rejected {
            status: 400,
            message: "Validation Failed".into()
        }
    );
    fn none(_: &serde_json::Value) -> Option<&str> {
        None
    }
    let c = ApiClient::new(
        &s.uri(),
        AuthScheme::Bearer(TOKEN.into()),
        ClientOpts {
            error_message: none,
            ..opts()
        },
    );
    assert_eq!(
        c.post_checked("/gh", None).await.unwrap_err(),
        ProviderError::Http(400)
    );
}

#[tokio::test]
async fn raw_get_returns_status_headers_and_body_and_sends_request_headers() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/c"))
        .and(header("if-none-match", "W/\"e1\""))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "W/\"e1\""))
        .expect(1)
        .mount(&s)
        .await;
    let c = ApiClient::new(&s.uri(), AuthScheme::Bearer(TOKEN.into()), opts());
    let r = c
        .get_raw(
            "/c",
            &[(
                HeaderName::from_static("if-none-match"),
                HeaderValue::from_static("W/\"e1\""),
            )],
        )
        .await
        .unwrap();
    assert_eq!(r.status, 304);
    assert_eq!(r.header("etag"), Some("W/\"e1\""));
    assert!(r.body.is_empty());
}

#[tokio::test]
async fn raw_requests_leave_429_to_the_caller_when_asked() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/g"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .expect(1)
        .mount(&s)
        .await;
    let c = ApiClient::new(
        &s.uri(),
        AuthScheme::Bearer(TOKEN.into()),
        ClientOpts {
            retry_429: false,
            ..opts()
        },
    );
    let r = c.post_raw("/g", &json!({"query": "{}"})).await.unwrap();
    assert_eq!(r.status, 429);
    assert_eq!(r.header("retry-after"), Some("0"));
    // a raw read still refuses another origin
    assert!(matches!(
        c.get_raw("https://elsewhere.example/x", &[]).await,
        Err(ProviderError::Network(_))
    ));
}

#[tokio::test]
async fn put_is_a_write_and_follows_no_redirect() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/w"))
        .respond_with(ResponseTemplate::new(307).insert_header("Location", "/w2"))
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path("/w2"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&s)
        .await;
    let c = ApiClient::new(&s.uri(), AuthScheme::Bearer(TOKEN.into()), opts());
    assert_eq!(
        c.put_checked("/w", Some(&json!({}))).await.unwrap_err(),
        ProviderError::Http(307)
    );
}
