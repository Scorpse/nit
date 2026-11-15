//! GitHubProvider comment reads against a fake GitHub (wiremock) serving the docs-derived
//! fixtures of `tests/fixtures/github/`.

use prcore::comments::{build_threads, Comment, CommentId, ThreadId};
use prcore::draft::Side;
use prcore::model::RepoRef;
use providers::github::GitHubProvider;
use providers::provider::{CommentList, Provider};
use providers::ProviderError;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/github/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn json(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(fixture(name), "application/json")
}

fn repo() -> RepoRef {
    RepoRef::parse("github.com", "octo-org/api").unwrap()
}

async fn server() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviewThreads(first: 50"))
        .respond_with(json("graphql_review_threads.json"))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("node(id: $id)"))
        .and(body_string_contains("PRRT_kwDOAthreadE"))
        .respond_with(json("graphql_thread_comments_page2.json"))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviews(first: 100"))
        .respond_with(json("graphql_reviews.json"))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/issues/1347/comments"))
        .respond_with(json("issue_comments.json"))
        .mount(&s)
        .await;
    s
}

async fn read(s: &MockServer) -> CommentList {
    GitHubProvider::new(
        &s.uri(),
        &format!("{}/graphql", s.uri()),
        "github.com",
        "tok",
    )
    .comments(&repo(), 1347)
    .await
    .unwrap()
}

fn get<'a>(l: &'a CommentList, id: &str) -> &'a Comment {
    l.comments
        .iter()
        .find(|c| c.id.as_str() == id)
        .unwrap_or_else(|| panic!("{id} missing"))
}

#[tokio::test]
async fn threads_keep_github_order_and_ids() {
    let s = server().await;
    let l = read(&s).await;
    assert!(l.skipped.is_empty(), "{:?}", l.skipped);
    assert_eq!(l.tasks, None);
    let root = get(&l, "rc:101");
    let reply = get(&l, "rc:102");
    let t = ThreadId("PRRT_kwDOAthreadA|101".into());
    assert_eq!(root.thread.as_ref(), Some(&t));
    assert_eq!(reply.thread.as_ref(), Some(&t));
    assert_eq!(root.parent_id, None);
    assert!(root.can_resolve && !root.resolved);
    assert_eq!(root.author.login.as_deref(), Some("octocat"));
    assert_eq!(root.author.id, "1");
    // grouped into one thread whose root is the earliest comment
    let threads = build_threads(l.comments.clone());
    let a = threads.iter().find(|th| th.id() == Some(&t)).unwrap();
    assert_eq!(a.root.id, CommentId("rc:101".into()));
    assert_eq!(a.replies.len(), 1);
    assert_eq!(a.replies[0].id, CommentId("rc:102".into()));
}

#[tokio::test]
async fn right_side_maps_to_to_and_left_side_maps_to_from() {
    let s = server().await;
    let l = read(&s).await;
    let right = get(&l, "rc:101").anchor.clone().unwrap();
    assert_eq!(
        (right.path.as_str(), right.from, right.to),
        ("src/main.rs", None, Some(12))
    );
    assert_eq!(right.start, None);
    assert_eq!(
        right.commit.as_deref(),
        Some("6dcb09b5b57875f334f61aebed695e2e4193db5e")
    );
    let left = get(&l, "rc:103").anchor.clone().unwrap();
    assert_eq!((left.from, left.to), (Some(7), None));
}

#[tokio::test]
async fn multi_line_start_side() {
    let s = server().await;
    let l = read(&s).await;
    let left = get(&l, "rc:103").anchor.clone().unwrap();
    assert_eq!(left.start, Some((Side::Old, 5)));
    // a range from a deleted line to an added line keeps both sides
    let across = get(&l, "rc:106").anchor.clone().unwrap();
    assert_eq!((across.to, across.start), (Some(22), Some((Side::Old, 19))));
}

#[tokio::test]
async fn outdated_thread_uses_original_line() {
    let s = server().await;
    let l = read(&s).await;
    let c = get(&l, "rc:104");
    let a = c.anchor.clone().unwrap();
    assert!(a.outdated);
    assert_eq!((a.from, a.to), (None, Some(30)));
    assert_eq!(
        a.commit.as_deref(),
        Some("0000000000000000000000000000000000000001")
    );
    assert!(c.resolved);
    // viewerCanUnresolve alone still allows the toggle
    assert!(c.can_resolve);
}

#[tokio::test]
async fn file_level_thread_has_no_lines() {
    let s = server().await;
    let l = read(&s).await;
    let c = get(&l, "rc:105");
    let a = c.anchor.clone().unwrap();
    assert_eq!(
        (a.path.as_str(), a.from, a.to, a.start),
        ("README.md", None, None, None)
    );
    assert!(!c.can_resolve);
    // a deleted author shows as unknown
    assert_eq!(c.author.display_name, prcore::comments::UNKNOWN_AUTHOR);
}

#[tokio::test]
async fn issue_comments_have_no_thread() {
    let s = server().await;
    let l = read(&s).await;
    let c = get(&l, "ic:1");
    assert_eq!((c.thread.as_ref(), c.anchor.as_ref()), (None, None));
    assert!(!c.can_resolve);
    assert_eq!(c.body, "Me too");
}

#[tokio::test]
async fn review_body_shown_as_pr_comment() {
    let s = server().await;
    let l = read(&s).await;
    let c = get(&l, "rv:80");
    assert_eq!(c.body, "Here is the body for the review.");
    assert_eq!((c.thread.as_ref(), c.anchor.as_ref()), (None, None));
    // a review without text adds nothing
    assert!(l.comments.iter().all(|c| c.id.as_str() != "rv:81"));
}

#[tokio::test]
async fn bot_author_detected() {
    let s = server().await;
    let l = read(&s).await;
    assert!(get(&l, "ic:2").author_is_bot, "type Bot / [bot] login");
    assert!(get(&l, "rc:104").author_is_bot, "GraphQL Bot actor");
    assert!(get(&l, "rc:107").author_is_bot);
    assert!(!get(&l, "rc:101").author_is_bot);
    assert!(!get(&l, "ic:1").author_is_bot);
}

#[tokio::test]
async fn nested_comment_paging_followed() {
    let s = server().await;
    let l = read(&s).await;
    let c = get(&l, "rc:107");
    assert_eq!(c.thread, Some(ThreadId("PRRT_kwDOAthreadE|106".into())));
    assert_eq!(c.body, "Second page reply.");
    // every comment of every thread, plus 2 issue comments and 1 review body
    assert_eq!(l.comments.len(), 7 + 2 + 1);
}

#[tokio::test]
async fn review_bodies_failing_does_not_fail_the_comments() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviewThreads(first: 50"))
        .respond_with(json("graphql_review_threads.json"))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("node(id: $id)"))
        .respond_with(json("graphql_thread_comments_page2.json"))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviews(first: 100"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/issues/1347/comments"))
        .respond_with(json("issue_comments.json"))
        .mount(&s)
        .await;
    let l = read(&s).await;
    assert_eq!(l.comments.len(), 7 + 2);
}

#[tokio::test]
async fn missing_pr_is_not_found() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(json("graphql_errors.json"))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    let err = GitHubProvider::new(&s.uri(), &format!("{}/graphql", s.uri()), "github.com", "t")
        .comments(&repo(), 1)
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::NotFound);
}
