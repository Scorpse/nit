//! GitLabProvider inbox reads against a fake GitLab (wiremock) serving the docs-derived
//! fixtures of `tests/fixtures/gitlab/` (see `tests/fixtures/gitlab/SOURCES.md`).

use std::time::{Duration, SystemTime};

use prcore::model::{Forge, PrState, RepoRef, ReviewState};
use providers::gitlab::{GitLabProvider, CAPPED_FILES, FULL_LIST_EVERY};
use providers::provider::{Provider, SkippedPr};
use providers::ProviderError;
use serde_json::Value;
use wiremock::matchers::{header, method, path, path_regex, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "glpat-test-T0KEN-not-real";
const HOST: &str = "gitlab.example.com";
const MRS: &str = "/projects/my-group%2Fsub%2Fmy-project/merge_requests";

fn provider(s: &MockServer) -> GitLabProvider {
    GitLabProvider::new(&s.uri(), HOST, TOKEN)
}

fn repo() -> RepoRef {
    RepoRef::parse(HOST, "my-group/sub/my-project").unwrap()
}

fn fixture(name: &str, vars: &[(&str, &str)]) -> String {
    let p = format!(
        "{}/tests/fixtures/gitlab/{name}",
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
        "{}/tests/fixtures/gitlab/{name}.headers.json",
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

fn empty_list() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw("[]", "application/json")
}

async fn mount_user(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(respond(200, "user", &[]))
        .mount(s)
        .await;
}

/// Reviewers and approvals of every MR: MR 10 asks me, MR 13 I approved, others none.
async fn mount_review_state(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/10/reviewers")))
        .respond_with(respond(200, "mr_reviewers", &[]))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/13/reviewers")))
        .respond_with(respond(200, "mr_reviewers_approved", &[]))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/merge_requests/\d+/reviewers$"))
        .respond_with(empty_list())
        .with_priority(10)
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/merge_requests/\d+/approvals$"))
        .respond_with(respond(200, "mr_approvals", &[]))
        .mount(s)
        .await;
}

async fn inbox_server() -> MockServer {
    let s = MockServer::start().await;
    mount_user(&s).await;
    mount_review_state(&s).await;
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(query_param("state", "opened"))
        .respond_with(respond(200, "mrs_opened", &[]))
        .mount(&s)
        .await;
    s
}

fn by_id(list: &providers::provider::PrList, id: u64) -> &prcore::model::PullRequest {
    list.prs.iter().find(|p| p.id == id).unwrap()
}

#[tokio::test]
async fn me_maps_numeric_id_and_username_with_private_token_header() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("private-token", TOKEN))
        .respond_with(respond(200, "user", &[]))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    let me = p.me().await.unwrap();
    assert_eq!(me.id, "1");
    assert_eq!(me.login.as_deref(), Some("root"));
    assert_eq!(me.display_name, "Administrator");
    assert_eq!(p.client().rate_limit(), Some((1999, 2000)));
    assert_eq!(p.forge(), Forge::GitLab);
}

#[tokio::test]
async fn discover_lists_nested_groups_by_link_then_x_next_page() {
    let s = MockServer::start().await;
    let base = s.uri();
    Mock::given(method("GET"))
        .and(path("/projects"))
        .and(query_param("page", "3"))
        .respond_with(respond(200, "projects_page3", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects"))
        .and(query_param("page", "2"))
        .respond_with(respond(200, "projects_page2", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects"))
        .and(query_param("membership", "true"))
        .and(query_param("archived", "false"))
        .and(query_param("simple", "true"))
        .and(query_param_is_missing("page"))
        .respond_with(respond(200, "projects_page1", &[("base", &base)]))
        .expect(1)
        .mount(&s)
        .await;
    let d = provider(&s).discover().await.unwrap();
    let keys: Vec<String> = d.repos.iter().map(|r| r.repo.key()).collect();
    assert_eq!(
        keys,
        [
            "gitlab.example.com/my-group/sub/my-project",
            "gitlab.example.com/my-group/tools",
            "gitlab.example.com/root/website"
        ]
    );
    assert_eq!(d.repos[0].repo.namespace, "my-group/sub");
    assert_eq!(d.namespaces, ["my-group", "my-group/sub", "root"]);
    assert_eq!(
        d.repos.iter().map(|r| r.private).collect::<Vec<_>>(),
        [Some(true), Some(true), Some(false)]
    );
    assert_eq!(d.repos[0].name.as_deref(), Some("My Project"));
}

#[tokio::test]
async fn x_next_page_and_link_paging_refuse_another_origin() {
    let s = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(empty_list())
        .expect(0)
        .mount(&other)
        .await;
    // a next link on another origin and no x-next-page: an error, never a silent stop
    let link = format!("<{}/projects?page=2>; rel=\"next\"", other.uri());
    Mock::given(method("GET"))
        .and(path("/projects"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("projects_page1.json", &[]), "application/json")
                .insert_header("link", link.as_str()),
        )
        .mount(&s)
        .await;
    assert!(matches!(
        provider(&s).discover().await,
        Err(ProviderError::Network(_))
    ));
}

/// Live GitLab 19.5.1 reached at `http://IP:8080` with `external_url` set to
/// `https://IP:8443`: every `Link` points at the external URL (another origin). The pages
/// come from `x-next-page` on the client's own base; the foreign link is never followed.
#[tokio::test]
async fn link_on_the_external_url_pages_by_x_next_page_on_the_own_origin() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/projects"))
        .and(query_param("page", "2"))
        .respond_with(respond(200, "projects_live_page2", &[]))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects"))
        .and(query_param_is_missing("page"))
        .respond_with(respond(200, "projects_live_page1", &[]))
        .expect(1)
        .mount(&s)
        .await;
    let p = GitLabProvider::new(&s.uri(), "192.168.1.20:8080", TOKEN);
    let d = p.discover().await.unwrap();
    let keys: Vec<String> = d.repos.iter().map(|r| r.repo.key()).collect();
    assert_eq!(
        keys,
        [
            "192.168.1.20:8080/root/nit-live-test",
            "192.168.1.20:8080/root/nit-live-test-2"
        ]
    );
    assert_eq!(d.repos[0].private, Some(true));
}

#[tokio::test]
async fn nested_namespace_path_is_percent_encoded_once() {
    let s = inbox_server().await;
    provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    for r in s.received_requests().await.unwrap() {
        let raw = r.url.path();
        if raw.starts_with("/projects/") {
            assert!(
                raw.starts_with("/projects/my-group%2Fsub%2Fmy-project/"),
                "{raw}"
            );
            assert!(!raw.contains("%252F"), "double-encoded: {raw}");
        }
    }
}

#[tokio::test]
async fn relative_url_api_base_is_used_for_every_call() {
    let s = MockServer::start().await;
    let prefix = "/gitlab/api/v4";
    Mock::given(method("GET"))
        .and(path(format!("{prefix}/user")))
        .respond_with(respond(200, "user", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(format!(
            "^{prefix}/projects/[^/]+/merge_requests$"
        )))
        .respond_with(respond(200, "mrs_opened", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/(reviewers|approvals)$"))
        .respond_with(empty_list())
        .mount(&s)
        .await;
    let host = "git.corp.example/gitlab";
    let p = GitLabProvider::new(&format!("{}{prefix}", s.uri()), host, TOKEN);
    let r = RepoRef::parse(host, "grp/proj").unwrap();
    let list = p.prs(&r, PrState::Open, usize::MAX).await.unwrap();
    assert_eq!(list.prs.len(), 4);
    assert_eq!(list.prs[0].repo.key(), "git.corp.example/gitlab/grp/proj");
    let reqs = s.received_requests().await.unwrap();
    assert!(!reqs.is_empty());
    for r in reqs {
        assert!(r.url.path().starts_with(prefix), "{}", r.url);
    }
}

#[tokio::test]
async fn updated_after_cursor_sent_and_advanced() {
    let s = MockServer::start().await;
    mount_user(&s).await;
    mount_review_state(&s).await;
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(query_param("updated_after", "2026-11-11T09:00:00.000Z"))
        .respond_with(empty_list())
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(query_param("updated_after", "2026-11-10T10:00:00.000Z"))
        .respond_with(respond(200, "mrs_updated", &[]))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(query_param("state", "opened"))
        .and(query_param_is_missing("updated_after"))
        .respond_with(respond(200, "mrs_opened", &[]))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    let first = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
    assert_eq!(first.prs.len(), 4);
    let second = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
    // MR 10 changed; the others are kept from the first list
    assert_eq!(second.prs.len(), 4);
    assert_eq!(by_id(&second, 10).title, "Add widgets (v2)");
    assert_eq!(
        by_id(&second, 10).source_commit,
        "44444444444444444444444444444444444444aa"
    );
    let third = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
    assert_eq!(third, second);
    // reviewers and approvals asked only for MRs that changed: 4 + 1
    let reqs = s.received_requests().await.unwrap();
    let reviewer_calls = reqs
        .iter()
        .filter(|r| r.url.path().ends_with("/reviewers"))
        .count();
    assert_eq!(reviewer_calls, 5);
}

#[tokio::test]
async fn periodic_full_list_drops_closed_mrs() {
    let s = MockServer::start().await;
    mount_user(&s).await;
    mount_review_state(&s).await;
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(query_param_is_missing("updated_after"))
        .respond_with(respond(200, "mrs_opened", &[]))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&s)
        .await;
    // every later full list: only MR 10 is still open
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(query_param_is_missing("updated_after"))
        .respond_with(respond(200, "mrs_updated", &[]))
        .with_priority(2)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(wiremock::matchers::query_param_contains(
            "updated_after",
            "2026",
        ))
        .respond_with(empty_list())
        .mount(&s)
        .await;
    let p = provider(&s);
    for _ in 0..FULL_LIST_EVERY {
        let l = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
        assert_eq!(l.prs.len(), 4, "closed MRs stay until the full list");
    }
    let full = p.prs(&repo(), PrState::Open, usize::MAX).await.unwrap();
    assert_eq!(full.prs.iter().map(|p| p.id).collect::<Vec<_>>(), [10]);
}

#[tokio::test]
async fn reviewer_states_map_to_review_state() {
    let s = inbox_server().await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    let mr = by_id(&list, 10);
    let states: Vec<(String, ReviewState)> = mr
        .reviewers
        .iter()
        .map(|r| (r.user.id.clone(), r.state.clone()))
        .collect();
    assert_eq!(
        states,
        [
            ("1".to_string(), ReviewState::Pending),
            ("2".to_string(), ReviewState::ChangesRequested),
            ("6".to_string(), ReviewState::Commented),
            // approved through /approvals without being a reviewer
            ("7".to_string(), ReviewState::Approved),
        ]
    );
    assert_eq!(mr.forge, Forge::GitLab);
    assert_eq!(mr.node_id.as_deref(), Some("3"));
    assert_eq!(
        mr.web_url,
        "https://gitlab.example.com/my-group/sub/my-project/-/merge_requests/10"
    );
    assert_eq!(mr.comment_count, 3);
    assert_eq!(mr.source_commit, "33e2ee8579fda5bc36accc9c6fbd0b4fefda9e30");
}

#[tokio::test]
async fn needs_me_from_reviews_for_me_and_state() {
    let s = inbox_server().await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    // I am an unreviewed reviewer
    assert!(by_id(&list, 10).review_requested_from_me);
    // fork MR, nobody asked me
    assert!(!by_id(&list, 11).review_requested_from_me);
    // my own draft
    let own = by_id(&list, 12);
    assert!(own.draft && !own.review_requested_from_me);
    // I already approved
    let approved = by_id(&list, 13);
    assert!(!approved.review_requested_from_me);
    assert_eq!(approved.reviewers[0].state, ReviewState::Approved);
}

#[tokio::test]
async fn fork_mr_flagged_is_fork() {
    let s = inbox_server().await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    assert!(by_id(&list, 11).is_fork());
    assert!(!by_id(&list, 10).is_fork());
}

#[tokio::test]
async fn bad_mr_entry_skipped_with_field_path() {
    let s = inbox_server().await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        list.skipped,
        [SkippedPr {
            id: Some(14),
            field: "title".into()
        }]
    );
}

#[tokio::test]
async fn changes_count_1000_plus_is_capped() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/10")))
        .respond_with(respond(200, "mr_detail", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/13")))
        .respond_with(respond(200, "mr_detail_1000plus", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/15")))
        .respond_with(respond(200, "mr_detail_empty_diff_refs", &[]))
        .mount(&s)
        .await;
    let p = provider(&s);
    let small = p.diffstat(&repo(), 10).await.unwrap();
    // files only (D6): lines are unknown until the MR is opened locally
    assert_eq!(
        (
            small.files,
            small.lines_added,
            small.lines_removed,
            small.max_file_lines
        ),
        (5, 0, 0, 0)
    );
    let big = p.diffstat(&repo(), 13).await.unwrap();
    assert_eq!(big.files, CAPPED_FILES);
    assert!(big.exceeds_web_ui(Forge::GitLab));
    // not computed yet: no size rather than a wrong one
    assert!(matches!(
        p.diffstat(&repo(), 15).await,
        Err(ProviderError::Decode(_))
    ));
}

#[tokio::test]
async fn pr_and_revision_from_the_detail_and_the_latest_version() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/10")))
        .respond_with(respond(200, "mr_detail", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/10/versions")))
        .respond_with(respond(200, "mr_versions", &[]))
        .mount(&s)
        .await;
    let p = provider(&s);
    let pr = p.pr(&repo(), 10).await.unwrap();
    assert_eq!((pr.id, &pr.state), (10, &PrState::Open));
    assert_eq!(pr.dest_branch, "main");
    let rev = p.revision(&repo(), 10).await.unwrap();
    assert_eq!(rev.head, "33e2ee8579fda5bc36accc9c6fbd0b4fefda9e30");
    assert_eq!(
        rev.base.as_deref(),
        Some("eeb57dffe83deb686a60a71c16c32f71046868fd")
    );
    assert_eq!(
        rev.start.as_deref(),
        Some("eeb57dffe83deb686a60a71c16c32f71046868fd")
    );
    assert_eq!(rev.dest_branch, "main");
}

#[tokio::test]
async fn empty_diff_refs_gives_no_base() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/15")))
        .respond_with(respond(200, "mr_detail_empty_diff_refs", &[]))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MRS}/15/versions")))
        .respond_with(empty_list())
        .mount(&s)
        .await;
    let rev = provider(&s).revision(&repo(), 15).await.unwrap();
    assert_eq!(rev.head, "7777777777777777777777777777777777777777");
    assert_eq!(rev.base, None);
    assert_eq!(rev.start, None);
}

#[tokio::test]
async fn merged_lookup_filters_by_source_branch_and_state() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(query_param("state", "merged"))
        .and(query_param("source_branch", "base-topic"))
        .respond_with(respond(200, "mrs_merged", &[]))
        .expect(1)
        .mount(&s)
        .await;
    let list = provider(&s)
        .merged_by_source_branch(&repo(), "base-topic")
        .await
        .unwrap();
    assert_eq!(list.prs.iter().map(|p| p.id).collect::<Vec<_>>(), [7]);
    assert_eq!(list.prs[0].state, PrState::Merged);
    assert_eq!(
        list.skipped,
        [SkippedPr {
            id: Some(5),
            field: "source_branch".into()
        }]
    );
}

#[tokio::test]
async fn closed_maps_to_declined() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(MRS))
        .and(query_param("state", "closed"))
        .respond_with(respond(200, "mrs_merged", &[]))
        .mount(&s)
        .await;
    let list = provider(&s)
        .prs(&repo(), PrState::Declined, 10)
        .await
        .unwrap();
    assert_eq!(list.prs.iter().map(|p| p.id).collect::<Vec<_>>(), [6]);
}

#[tokio::test]
async fn retry_after_429_honoured() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(respond(429, "rate_limit_429", &[("retry_after", "0")]))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&s)
        .await;
    mount_user(&s).await;
    let p = provider(&s);
    assert_eq!(p.me().await.unwrap().id, "1");
    assert!(p.client().paused_until().is_none());

    // a long wait is not taken in place: the account pauses and fails fast
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(respond(429, "rate_limit_429", &[("retry_after", "120")]))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    let started = std::time::Instant::now();
    assert_eq!(p.me().await.unwrap_err(), ProviderError::RateLimited);
    assert!(started.elapsed() < Duration::from_secs(5));
    let left = p
        .client()
        .paused_until()
        .expect("paused")
        .duration_since(SystemTime::now())
        .unwrap();
    assert!(left > Duration::from_secs(100), "{left:?}");
    assert_eq!(p.me().await.unwrap_err(), ProviderError::RateLimited);
}

#[tokio::test]
async fn unauthorized_is_auth() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(respond(401, "unauthorized_401", &[]))
        .mount(&s)
        .await;
    assert_eq!(provider(&s).me().await.unwrap_err(), ProviderError::Auth);
}

/// GitLab has no MR tasks: refused before any request (writes: `tests/gitlab_write.rs`).
#[tokio::test]
async fn tasks_are_refused_before_any_request() {
    let s = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&s)
        .await;
    let p = provider(&s);
    assert!(!p.capabilities().tasks);
    assert!(matches!(
        p.tasks(&repo(), 10).await,
        Err(ProviderError::Invalid(_))
    ));
}

#[test]
fn debug_never_contains_token() {
    let p = GitLabProvider::new("https://gitlab.com/api/v4", "gitlab.com", TOKEN);
    let shown = format!("{p:?} {:?}", p.client());
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(shown.contains("***"), "{shown}");
}
