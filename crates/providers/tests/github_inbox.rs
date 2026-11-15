//! GitHubProvider inbox reads against a fake GitHub (wiremock) serving the docs-derived
//! fixtures of `tests/fixtures/github/` (see `tests/fixtures/github/SOURCES.md`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prcore::model::{PrState, RepoRef, ReviewState};
use providers::github::{GitHubProvider, SSO_HINT};
use providers::provider::{Provider, SkippedPr};
use providers::ProviderError;
use serde_json::Value;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
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

fn repo() -> RepoRef {
    RepoRef::parse("github.com", "octo-org/api").unwrap()
}

/// A fixture body, with `{base}` and other placeholders filled in.
fn fixture(name: &str, vars: &[(&str, &str)]) -> String {
    let p = format!(
        "{}/tests/fixtures/github/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut text = std::fs::read_to_string(p).unwrap();
    for (k, v) in vars {
        text = text.replace(&format!("{{{k}}}"), v);
    }
    text
}

/// A response from `<name>.json` plus `<name>.headers.json` when it exists.
fn respond(status: u16, name: &str, vars: &[(&str, &str)]) -> ResponseTemplate {
    let mut r = ResponseTemplate::new(status)
        .set_body_raw(fixture(&format!("{name}.json"), vars), "application/json");
    let hp = format!(
        "{}/tests/fixtures/github/{name}.headers.json",
        env!("CARGO_MANIFEST_DIR")
    );
    if std::path::Path::new(&hp).exists() {
        let h: Value =
            serde_json::from_str(&fixture(&format!("{name}.headers.json"), vars)).unwrap();
        for (k, v) in h.as_object().unwrap() {
            r = r.insert_header(k.as_str(), v.as_str().unwrap());
        }
    }
    r
}

async fn mount_user(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(respond(200, "user", &[]))
        .mount(s)
        .await;
}

async fn mount_teams(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/user/teams"))
        .respond_with(respond(200, "user_teams", &[]))
        .mount(s)
        .await;
}

const PULLS: &str = "/repos/octo-org/api/pulls";

async fn mount_open_list(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path(PULLS))
        .and(query_param("state", "open"))
        .respond_with(respond(200, "pulls_open", &[]))
        .mount(s)
        .await;
}

fn graphql_prs() -> Mock {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("pullRequests(states: OPEN"))
        .respond_with(respond(200, "graphql_prs", &[]))
}

async fn inbox_server() -> MockServer {
    let s = MockServer::start().await;
    mount_user(&s).await;
    mount_teams(&s).await;
    mount_open_list(&s).await;
    graphql_prs().mount(&s).await;
    s
}

fn by_id(list: &providers::provider::PrList, id: u64) -> &prcore::model::PullRequest {
    list.prs.iter().find(|p| p.id == id).unwrap()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn me_maps_numeric_id_and_login() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(respond(200, "user", &[]))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    let me = p.me().await.unwrap();
    assert_eq!(me.id, "1");
    assert_eq!(me.login.as_deref(), Some("octocat"));
    assert_eq!(me.display_name, "monalisa octocat");
    assert_eq!(p.client().oauth_scopes().as_deref(), Some("repo, read:org"));
    assert_eq!(p.client().rate_limit(), Some((4999, 5000)));
}

#[tokio::test]
async fn discover_pages_by_link_header() {
    let s = MockServer::start().await;
    let base = s.uri();
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("page", "2"))
        .respond_with(respond(200, "user_repos_page2", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param(
            "affiliation",
            "owner,collaborator,organization_member",
        ))
        .respond_with(respond(200, "user_repos_page1", &[("base", &base)]))
        .mount(&s)
        .await;
    let d = provider(&s).discover().await.unwrap();
    let names: Vec<String> = d.repos.iter().map(|r| r.repo.key()).collect();
    assert_eq!(
        names,
        [
            "github.com/octocat/hello-world",
            "github.com/octo-org/api",
            "github.com/octo-org/web"
        ]
    );
    assert_eq!(d.namespaces, ["octo-org", "octocat"]);
    assert!(d.failed.is_empty());
    assert_eq!(d.repos[1].private, Some(true));
    assert_eq!(d.repos[0].name.as_deref(), Some("Hello-World"));
}

#[tokio::test]
async fn link_header_to_another_origin_is_refused() {
    let s = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(0)
        .mount(&other)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(respond(200, "user_repos_page1", &[("base", &other.uri())]))
        .mount(&s)
        .await;
    assert!(matches!(
        provider(&s).discover().await,
        Err(ProviderError::Network(_))
    ));
}

#[tokio::test]
async fn prs_304_makes_no_graphql_call() {
    let s = MockServer::start().await;
    mount_user(&s).await;
    mount_teams(&s).await;
    // the second list request carries the ETag of the first and gets 304
    Mock::given(method("GET"))
        .and(path(PULLS))
        .and(header(
            "if-none-match",
            "W/\"a00049ba79152d03380c34652f2cb612\"",
        ))
        .respond_with(ResponseTemplate::new(304))
        .expect(2)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(respond(200, "pulls_open", &[]))
        .expect(1)
        .mount(&s)
        .await;
    graphql_prs().expect(1).mount(&s).await;
    let p = provider(&s);
    let first = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
    let second = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first.prs.len(), 5);
    // the 304 is not counted against the primary limit
    let before = p.client().request_count();
    p.prs(&repo(), PrState::Open, usize::MAX).await.ok();
    assert_eq!(p.client().request_count(), before);
}

#[tokio::test]
async fn prs_fresh_list_triggers_one_graphql_query_per_repo_and_none_when_empty() {
    let s = inbox_server().await;
    let p = provider(&s);
    p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
    let graphql =
        |reqs: &[wiremock::Request]| reqs.iter().filter(|r| r.url.path() == "/graphql").count();
    assert_eq!(graphql(&s.received_requests().await.unwrap()), 1);

    let empty = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&empty)
        .await;
    graphql_prs().expect(0).mount(&empty).await;
    let list = provider(&empty)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    assert!(list.prs.is_empty() && list.skipped.is_empty());
}

#[tokio::test]
async fn sizes_come_from_graphql() {
    let s = inbox_server().await;
    Mock::given(method("GET"))
        .and(path(format!("{PULLS}/1347")))
        .respond_with(respond(200, "pull_detail", &[]))
        .expect(0)
        .mount(&s)
        .await;
    let p = provider(&s);
    let list = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
    let pr = by_id(&list, 1347);
    assert_eq!(pr.forge, prcore::model::Forge::GitHub);
    assert_eq!(pr.web_url, "https://github.com/octo-org/api/pull/1347");
    assert_eq!(pr.node_id.as_deref(), Some("PR_kwDOA1347"));
    assert_eq!(pr.source_branch, "new-topic");
    assert_eq!(pr.dest_branch, "main");
    assert_eq!(pr.source_commit, "6dcb09b5b57875f334f61aebed695e2e4193db5e");
    assert_eq!(pr.comment_count, 6);
    let stat = p.diffstat(&repo(), 1347).await.unwrap();
    assert_eq!(
        (
            stat.files,
            stat.lines_added,
            stat.lines_removed,
            stat.max_file_lines
        ),
        (5, 100, 3, 0)
    );
    // the 3100-file PR is over the API's file-list cap
    assert!(p
        .diffstat(&repo(), 1351)
        .await
        .unwrap()
        .exceeds_web_ui(prcore::model::Forge::GitHub));
}

#[tokio::test]
async fn diffstat_falls_back_to_the_rest_detail() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{PULLS}/1347")))
        .respond_with(respond(200, "pull_detail", &[]))
        .expect(1)
        .mount(&s)
        .await;
    let stat = provider(&s).diffstat(&repo(), 1347).await.unwrap();
    assert_eq!(
        (stat.files, stat.lines_added, stat.lines_removed),
        (5, 100, 3)
    );
}

#[tokio::test]
async fn requested_reviewer_via_team_or_user_sets_needs_me() {
    let s = inbox_server().await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    let direct = by_id(&list, 1347);
    assert!(direct.review_requested_from_me);
    assert!(direct
        .reviewers
        .iter()
        .any(|r| r.user.id == "1" && r.state == ReviewState::Pending));
    let team = by_id(&list, 1348);
    assert!(team.review_requested_from_me, "team octo-org/reviewers");
    assert_eq!(team.reviewers[0].user.id, "team:octo-org/reviewers");
}

#[tokio::test]
async fn team_request_without_team_access_is_not_needs_me() {
    let s = MockServer::start().await;
    mount_user(&s).await;
    mount_open_list(&s).await;
    graphql_prs().mount(&s).await;
    Mock::given(method("GET"))
        .and(path("/user/teams"))
        .respond_with(respond(403, "fine_grained_403", &[]))
        .mount(&s)
        .await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    assert!(!by_id(&list, 1348).review_requested_from_me);
    assert!(by_id(&list, 1347).review_requested_from_me);
}

#[tokio::test]
async fn reviewed_and_not_requested_is_not_needs_me_unless_new_commits() {
    let s = inbox_server().await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    // commented on an older head: new commits want another look
    let pushed = by_id(&list, 1349);
    assert!(pushed.review_requested_from_me);
    assert_eq!(pushed.reviewers[0].state, ReviewState::Commented);
    // approved (on an older head): not needed again
    let approved = by_id(&list, 1350);
    assert!(!approved.review_requested_from_me);
    let states: Vec<ReviewState> = approved.reviewers.iter().map(|r| r.state.clone()).collect();
    assert_eq!(
        states,
        [ReviewState::Approved, ReviewState::ChangesRequested]
    );
    // my own PR never needs me
    let own = by_id(&list, 1351);
    assert!(own.draft && !own.review_requested_from_me);
}

#[tokio::test]
async fn fork_pr_flagged_is_fork() {
    let s = inbox_server().await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    let fork = by_id(&list, 1350);
    assert!(fork.is_fork());
    assert_eq!(fork.source_repo.key(), "github.com/forker/api");
    // no head repository but not cross-repository: the base repository
    assert!(!by_id(&list, 1351).is_fork());
    assert!(!by_id(&list, 1347).is_fork());
}

#[tokio::test]
async fn bad_pr_entry_skipped_with_field_path() {
    let s = inbox_server().await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        list.skipped,
        [SkippedPr {
            id: Some(1352),
            field: "title".into()
        }]
    );
}

#[tokio::test]
async fn closed_unmerged_maps_to_declined() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .and(query_param("state", "closed"))
        .respond_with(respond(200, "pulls_closed_head", &[]))
        .mount(&s)
        .await;
    let p = provider(&s);
    let declined = p.prs(&repo(), PrState::Declined, 10).await.unwrap();
    let ids: Vec<u64> = declined.prs.iter().map(|p| p.id).collect();
    assert_eq!(ids, [1299]);
    assert_eq!(declined.prs[0].state, PrState::Declined);
    let merged = p.prs(&repo(), PrState::Merged, 10).await.unwrap();
    assert_eq!(merged.prs.iter().map(|p| p.id).collect::<Vec<_>>(), [1300]);
}

#[tokio::test]
async fn merged_lookup_filters_by_head_and_merged_at() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .and(query_param("state", "closed"))
        .and(query_param("head", "octo-org:base-topic"))
        .respond_with(respond(200, "pulls_closed_head", &[]))
        .expect(1)
        .mount(&s)
        .await;
    let list = provider(&s)
        .merged_by_source_branch(&repo(), "base-topic")
        .await
        .unwrap();
    assert_eq!(list.prs.len(), 1);
    let m = &list.prs[0];
    assert_eq!((m.id, &m.state), (1300, &PrState::Merged));
    assert_eq!(m.source_branch, "base-topic");
    assert_eq!(
        list.skipped,
        [SkippedPr {
            id: Some(1298),
            field: "head.ref".into()
        }]
    );
}

#[tokio::test]
async fn rate_limit_403_waits_then_fails_with_paused_until() {
    let s = MockServer::start().await;
    let reset = (now_secs() + 3600).to_string();
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(respond(403, "rate_limit_403", &[("reset", &reset)]))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    assert_eq!(p.me().await.unwrap_err(), ProviderError::RateLimited);
    let until = p.client().paused_until().expect("paused");
    let left = until.duration_since(SystemTime::now()).unwrap();
    assert!(left > Duration::from_secs(3500) && left <= Duration::from_secs(3601));
    // paused: fails fast without another request (expect(1) above)
    assert_eq!(p.me().await.unwrap_err(), ProviderError::RateLimited);
}

#[tokio::test]
async fn short_secondary_limit_is_waited_once_then_retried() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(respond(
            403,
            "secondary_rate_limit_403",
            &[("retry_after", "0")],
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&s)
        .await;
    mount_user(&s).await;
    let p = provider(&s);
    assert_eq!(p.me().await.unwrap().id, "1");
    assert!(p.client().paused_until().is_none());
}

#[tokio::test]
async fn long_429_pauses_without_waiting() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(respond(
            429,
            "secondary_rate_limit_403",
            &[("retry_after", "120")],
        ))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    let started = std::time::Instant::now();
    assert_eq!(p.me().await.unwrap_err(), ProviderError::RateLimited);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(p.client().paused_until().is_some());
}

#[tokio::test]
async fn sso_403_gives_sso_hint() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(respond(403, "sso_403", &[]))
        .mount(&s)
        .await;
    let p = provider(&s);
    let err = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap_err();
    assert_eq!(
        err,
        ProviderError::Rejected {
            status: 403,
            message: SSO_HINT.into()
        }
    );
    // the authorization URL is kept for the UI, never in the error text
    assert!(!err.user_message().contains("https://"));
    assert_eq!(
        p.client().sso_url().as_deref(),
        Some("https://github.com/orgs/octo-org/sso?authorization_request=EXAMPLE0000")
    );
}

#[tokio::test]
async fn fine_grained_403_is_forbidden_and_keeps_the_permission_hint() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(respond(403, "fine_grained_403", &[]))
        .mount(&s)
        .await;
    let p = provider(&s);
    assert_eq!(
        p.prs(&repo(), PrState::Open, 1).await.unwrap_err(),
        ProviderError::Forbidden
    );
    assert_eq!(
        p.client().accepted_permissions().as_deref(),
        Some("pull_requests=read")
    );
}

#[tokio::test]
async fn graphql_errors_map_to_fixed_errors() {
    let s = MockServer::start().await;
    mount_user(&s).await;
    mount_open_list(&s).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(respond(200, "graphql_errors", &[]))
        .mount(&s)
        .await;
    assert_eq!(
        provider(&s)
            .prs(&repo(), PrState::Open, usize::MAX)
            .await
            .unwrap_err(),
        ProviderError::NotFound
    );
}

#[tokio::test]
async fn pr_and_revision_from_the_rest_detail() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{PULLS}/1347")))
        .respond_with(respond(200, "pull_detail", &[]))
        .mount(&s)
        .await;
    let p = provider(&s);
    let pr = p.pr(&repo(), 1347).await.unwrap();
    assert_eq!(pr.state, PrState::Open);
    assert_eq!(pr.comment_count, 10);
    let rev = p.revision(&repo(), 1347).await.unwrap();
    assert_eq!(rev.head, "6dcb09b5b57875f334f61aebed695e2e4193db5e");
    assert_eq!(rev.dest_branch, "main");
    assert_eq!(rev.base, None);
}

#[test]
fn debug_never_contains_token() {
    let p = GitHubProvider::new(
        "https://api.github.com",
        "https://api.github.com/graphql",
        "github.com",
        TOKEN,
    );
    let shown = format!("{p:?}");
    assert!(!shown.contains(TOKEN), "{shown}");
}
