//! The "requested elsewhere" search (decision D4) against a fake GitHub (wiremock): one
//! request per minute at most, repositories named from `repository_url`.

use prcore::model::RepoRef;
use providers::github::GitHubProvider;
use providers::provider::Provider;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "gh-test-T0KEN-not-real";

fn provider(s: &MockServer) -> GitHubProvider {
    GitHubProvider::new(
        &s.uri(),
        &format!("{}/graphql", s.uri()),
        "github.com",
        TOKEN,
    )
}

fn item(base: &str, repo: &str, n: u64) -> serde_json::Value {
    json!({
        "number": n,
        "repository_url": format!("{base}/repos/{repo}"),
        "pull_request": { "url": format!("{base}/repos/{repo}/pulls/{n}") },
    })
}

#[tokio::test]
async fn search_rate_bounded() {
    let s = MockServer::start().await;
    let base = s.uri();
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .and(query_param(
            "q",
            "is:pr is:open review-requested:@me archived:false",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 3,
            "incomplete_results": false,
            "items": [
                item(&base, "octo-org/api", 1),
                item(&base, "octo-org/api", 2),
                item(&base, "other/web", 7),
            ],
        })))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    let found = p.review_requests_elsewhere().await.unwrap();
    let r = |n: &str| RepoRef::parse("github.com", n).unwrap();
    assert_eq!(
        found,
        Some(vec![r("octo-org/api"), r("octo-org/api"), r("other/web")]),
        "one entry per pull request, on the account's host"
    );
    // asked again within the minute (another sync): nothing is sent
    for _ in 0..3 {
        assert_eq!(p.review_requests_elsewhere().await.unwrap(), None);
    }
    // wiremock checks `expect(1)` when the server drops
}

#[tokio::test]
async fn search_failure_is_an_error_and_still_counts_against_the_minute() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .respond_with(
            ResponseTemplate::new(422).set_body_json(json!({"message": "Validation Failed"})),
        )
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    assert!(p.review_requests_elsewhere().await.is_err());
    assert_eq!(p.review_requests_elsewhere().await.unwrap(), None);
}
