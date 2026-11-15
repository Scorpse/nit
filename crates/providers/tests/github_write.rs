//! GitHubProvider writes against a fake GitHub (wiremock): one exact request body per
//! mapping (plan Task 19), refusals before any request, lost answers, 422, spacing.
//!
//! The `live_*` test at the bottom writes to a real throwaway PR and is never run by default
//! (`#[ignore]`, see its doc comment).

use std::time::{Duration, Instant};

use prcore::comments::{NewComment, ReviewAction, ThreadId};
use prcore::draft::Side;
use prcore::model::RepoRef;
use providers::github::{graphql, GitHubProvider, MIN_WRITE_SPACING};
use providers::provider::{set_review_checked, PostRevision, Provider};
use providers::ProviderError;
use serde_json::{json, Value};
use wiremock::matchers::{any, body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "gh-test-T0KEN-not-real";
const HEAD: &str = "6dcb09b5b57875f334f61aebed695e2e4193db5e";
const PULLS: &str = "/repos/octo-org/api/pulls/1347";

fn provider(s: &MockServer) -> GitHubProvider {
    provider_at(&s.uri())
}

fn provider_at(base: &str) -> GitHubProvider {
    GitHubProvider::new(base, &format!("{base}/graphql"), "github.com", TOKEN)
}

fn repo() -> RepoRef {
    RepoRef::parse("github.com", "octo-org/api").unwrap()
}

fn at() -> PostRevision {
    PostRevision {
        head: HEAD.into(),
        merge_base: "1111111111111111111111111111111111111111".into(),
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/github/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// A review comment as `POST /pulls/{n}/comments` echoes it (shape from the docs).
fn review_comment(id: u64, extra: Value) -> Value {
    let mut v = json!({
        "id": id,
        "node_id": "PRRC_kwDOnew",
        "pull_request_review_id": 42,
        "path": "src/lib.rs",
        "commit_id": HEAD,
        "original_commit_id": HEAD,
        "user": {"login": "octocat", "id": 1, "type": "User"},
        "body": "x",
        "created_at": "2026-11-14T10:00:00Z",
        "updated_at": "2026-11-14T10:00:00Z",
        "line": null, "side": null, "start_line": null, "start_side": null,
        "subject_type": "line"
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v
}

/// Nothing may reach the server.
async fn no_requests() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&s)
        .await;
    s
}

/// `GET /user` as `user_id`, `GET /pulls/1347` from the fixture (author hubot, id 2).
async fn mount_me_and_pr(s: &MockServer, user_id: u64) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"login": "someone", "id": user_id, "type": "User"})),
        )
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path(PULLS))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("pull_detail.json"), "application/json"),
        )
        .mount(s)
        .await;
}

// ---- comments ----

#[tokio::test]
async fn pr_comment_posts_an_issue_comment() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo-org/api/issues/1347/comments"))
        .and(body_json(json!({"body": "Looks good"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 77, "node_id": "IC_x", "body": "Looks good",
            "user": {"login": "octocat", "id": 1, "type": "User"},
            "created_at": "2026-11-14T10:00:00Z", "updated_at": "2026-11-14T10:00:00Z"
        })))
        .expect(1)
        .mount(&s)
        .await;
    let c = provider(&s)
        .post_comment(
            &repo(),
            1347,
            &at(),
            &NewComment::Pr {
                text: "Looks good".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(c.id.as_str(), "ic:77");
    assert_eq!((c.thread, c.anchor), (None, None));
    assert_eq!(c.body, "Looks good");
    assert_eq!(c.author.id, "1");
}

#[tokio::test]
async fn file_comment_posts_subject_type_file_on_the_reviewed_head() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PULLS}/comments")))
        .and(body_json(json!({
            "body": "whole file",
            "commit_id": HEAD,
            "path": "src/lib.rs",
            "subject_type": "file"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(review_comment(
            501,
            json!({"subject_type": "file", "body": "whole file"}),
        )))
        .expect(1)
        .mount(&s)
        .await;
    let c = provider(&s)
        .post_comment(
            &repo(),
            1347,
            &at(),
            &NewComment::File {
                path: "src/lib.rs".into(),
                text: "whole file".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(c.id.as_str(), "rc:501");
    let a = c.anchor.unwrap();
    assert_eq!((a.path.as_str(), a.from, a.to), ("src/lib.rs", None, None));
}

#[tokio::test]
async fn inline_comment_sends_line_and_side_never_position() {
    let s = MockServer::start().await;
    // exact body: no `position`, no start fields for one line
    Mock::given(method("POST"))
        .and(path(format!("{PULLS}/comments")))
        .and(body_json(json!({
            "body": "nit",
            "commit_id": HEAD,
            "path": "src/lib.rs",
            "line": 12,
            "side": "RIGHT"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(review_comment(
            502,
            json!({"line": 12, "side": "RIGHT", "body": "nit"}),
        )))
        .expect(1)
        .mount(&s)
        .await;
    let c = provider(&s)
        .post_comment(
            &repo(),
            1347,
            &at(),
            &NewComment::Inline {
                path: "src/lib.rs".into(),
                side: Side::New,
                line: 12,
                start: None,
                text: "nit".into(),
            },
        )
        .await
        .unwrap();
    let a = c.anchor.unwrap();
    assert_eq!((a.from, a.to, a.start), (None, Some(12), None));
    assert_eq!(a.commit.as_deref(), Some(HEAD));
}

#[tokio::test]
async fn multi_line_inline_sends_start_line_and_start_side() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PULLS}/comments")))
        .and(body_json(json!({
            "body": "range",
            "commit_id": HEAD,
            "path": "src/lib.rs",
            "line": 9,
            "side": "RIGHT",
            "start_line": 7,
            "start_side": "LEFT"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(review_comment(
            503,
            json!({"line": 9, "side": "RIGHT", "start_line": 7, "start_side": "LEFT"}),
        )))
        .expect(1)
        .mount(&s)
        .await;
    let c = provider(&s)
        .post_comment(
            &repo(),
            1347,
            &at(),
            &NewComment::Inline {
                path: "src/lib.rs".into(),
                side: Side::New,
                line: 9,
                start: Some((Side::Old, 7)),
                text: "range".into(),
            },
        )
        .await
        .unwrap();
    let a = c.anchor.unwrap();
    assert_eq!((a.to, a.start), (Some(9), Some((Side::Old, 7))));
}

#[tokio::test]
async fn inline_line_zero_is_invalid_before_any_request() {
    let s = no_requests().await;
    let err = provider(&s)
        .post_comment(
            &repo(),
            1347,
            &at(),
            &NewComment::Inline {
                path: "src/lib.rs".into(),
                side: Side::New,
                line: 0,
                start: None,
                text: "x".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");
}

#[tokio::test]
async fn reply_posts_to_the_root_comment_of_the_thread() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PULLS}/comments/101/replies")))
        .and(body_json(json!({"body": "done"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(review_comment(
            504,
            json!({"in_reply_to_id": 101, "line": 5, "side": "RIGHT", "body": "done"}),
        )))
        .expect(1)
        .mount(&s)
        .await;
    let thread = ThreadId("PRRT_kwDOAthreadA|101".into());
    let c = provider(&s)
        .post_comment(
            &repo(),
            1347,
            &at(),
            &NewComment::Reply {
                thread: thread.clone(),
                text: "done".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(c.id.as_str(), "rc:504");
    // the reply joins the thread it answers
    assert_eq!(c.thread, Some(thread));
}

#[tokio::test]
async fn reply_without_a_root_comment_id_is_invalid_before_any_request() {
    let s = no_requests().await;
    for t in ["PRRT_kwDOAthreadA", "PRRT_kwDOAthreadA|", "PRRT_x|abc"] {
        let err = provider(&s)
            .post_comment(
                &repo(),
                1347,
                &at(),
                &NewComment::Reply {
                    thread: ThreadId(t.into()),
                    text: "x".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{t}: {err:?}");
    }
}

// ---- resolve ----

#[tokio::test]
async fn resolve_uses_graphql_thread_node_id() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_json(json!({
            "query": graphql::RESOLVE_THREAD,
            "variables": {"threadId": "PRRT_kwDOAthreadA"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"resolveReviewThread": {"thread": {"id": "PRRT_kwDOAthreadA", "isResolved": true}}}
        })))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_json(json!({
            "query": graphql::UNRESOLVE_THREAD,
            "variables": {"threadId": "PRRT_kwDOAthreadA"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"unresolveReviewThread": {"thread": {"id": "PRRT_kwDOAthreadA", "isResolved": false}}}
        })))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    let t = ThreadId("PRRT_kwDOAthreadA|101".into());
    p.set_resolved(&repo(), 1347, &t, true).await.unwrap();
    p.set_resolved(&repo(), 1347, &t, false).await.unwrap();
}

#[tokio::test]
async fn resolve_graphql_error_is_not_ok() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"resolveReviewThread": null},
            "errors": [{"type": "FORBIDDEN", "message": "Resource not accessible by integration"}]
        })))
        .expect(1)
        .mount(&s)
        .await;
    let err = provider(&s)
        .set_resolved(&repo(), 1347, &ThreadId("PRRT_x|1".into()), true)
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Forbidden);
}

// ---- reviews ----

#[tokio::test]
async fn approve_posts_a_review_on_the_head() {
    let s = MockServer::start().await;
    mount_me_and_pr(&s, 1).await;
    Mock::given(method("POST"))
        .and(path(format!("{PULLS}/reviews")))
        .and(body_json(json!({"event": "APPROVE", "commit_id": HEAD})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": 80, "state": "APPROVED"})),
        )
        .expect(1)
        .mount(&s)
        .await;
    set_review_checked(&provider(&s), &repo(), 1347, ReviewAction::Approve, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn approve_with_text_sends_the_body() {
    let s = MockServer::start().await;
    mount_me_and_pr(&s, 1).await;
    Mock::given(method("POST"))
        .and(path(format!("{PULLS}/reviews")))
        .and(body_json(
            json!({"event": "APPROVE", "body": "LGTM", "commit_id": HEAD}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 81})))
        .expect(1)
        .mount(&s)
        .await;
    set_review_checked(
        &provider(&s),
        &repo(),
        1347,
        ReviewAction::Approve,
        Some("LGTM"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn request_changes_posts_a_review_with_its_body() {
    let s = MockServer::start().await;
    mount_me_and_pr(&s, 1).await;
    Mock::given(method("POST"))
        .and(path(format!("{PULLS}/reviews")))
        .and(body_json(json!({
            "event": "REQUEST_CHANGES",
            "body": "Please add a test.",
            "commit_id": HEAD
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 82})))
        .expect(1)
        .mount(&s)
        .await;
    set_review_checked(
        &provider(&s),
        &repo(),
        1347,
        ReviewAction::RequestChanges,
        Some("Please add a test."),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn request_changes_without_body_is_invalid() {
    let s = no_requests().await;
    for body in [None, Some(""), Some("  \n ")] {
        let err = set_review_checked(
            &provider(&s),
            &repo(),
            1347,
            ReviewAction::RequestChanges,
            body,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");
    }
}

#[tokio::test]
async fn own_pr_approve_refused_without_request() {
    let s = MockServer::start().await;
    // I am hubot (id 2), the PR's author
    mount_me_and_pr(&s, 2).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&s)
        .await;
    let p = provider(&s);
    for (a, body) in [
        (ReviewAction::Approve, None),
        (ReviewAction::RequestChanges, Some("x")),
    ] {
        let err = set_review_checked(&p, &repo(), 1347, a, body)
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{a:?}: {err:?}");
    }
}

#[tokio::test]
async fn clearing_a_review_is_invalid_before_any_request() {
    let s = no_requests().await;
    let p = provider(&s);
    for a in [
        ReviewAction::ClearApproval,
        ReviewAction::ClearRequestChanges,
    ] {
        assert!(!p.capabilities().allows(a));
        // the provider refuses them too, not only the capability check
        let err = p.set_review(&repo(), 1347, a, Some("x")).await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{a:?}: {err:?}");
    }
}

#[test]
fn github_reports_its_write_capabilities() {
    let p = GitHubProvider::new(
        "https://api.github.com",
        "https://api.github.com/graphql",
        "github.com",
        TOKEN,
    );
    let caps = p.capabilities();
    assert!(!caps.read_only);
    assert!(caps.allows(ReviewAction::Approve) && caps.allows(ReviewAction::RequestChanges));
    assert!(!caps.allows(ReviewAction::ClearApproval));
    assert!(!caps.allows(ReviewAction::ClearRequestChanges));
    assert!(caps.resolve_threads && caps.file_comments);
    // GitHub issue comments take no replies
    assert!(!caps.reply_to_general_comments);
    // inline posting was enabled by the owner after the live anchoring run
    const _: () = assert!(providers::github::INLINE_ENABLED_GITHUB);
}

// ---- outcomes ----

/// A one-shot server that reads one whole request and closes without answering.
async fn dropping_server() -> String {
    use tokio::io::AsyncReadExt;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = l.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    break;
                }
            }
        }
        drop(sock);
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn dropped_connection_after_send_is_unconfirmed() {
    let base = dropping_server().await;
    let err = provider_at(&base)
        .post_comment(&repo(), 1347, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Unconfirmed);
    // the GraphQL resolve mutation too
    let base = dropping_server().await;
    let err = provider_at(&base)
        .set_resolved(&repo(), 1347, &ThreadId("PRRT_x|1".into()), true)
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Unconfirmed);
}

#[tokio::test]
async fn server_error_after_send_is_unconfirmed_and_not_retried() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1347, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Unconfirmed);
}

#[tokio::test]
async fn unreadable_2xx_is_posted_unreadable() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201).set_body_string("<html>ok</html>"))
        .expect(1)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1347, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::PostedUnreadable);
}

#[tokio::test]
async fn spam_422_is_rejected_not_retried() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PULLS}/comments")))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Validation Failed",
            "errors": [{"resource": "PullRequestReviewComment", "code": "custom",
                        "message": "was submitted too quickly\n(endpoint has been spammed)"}],
            "documentation_url": "https://docs.github.com/rest/pulls/comments"
        })))
        .expect(1)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(
            &repo(),
            1347,
            &at(),
            &NewComment::File {
                path: "src/lib.rs".into(),
                text: "x".into(),
            },
        )
        .await
        .unwrap_err();
    match err {
        ProviderError::Rejected { status, message } => {
            assert_eq!(status, 422);
            assert_eq!(
                message,
                "Validation Failed: was submitted too quickly (endpoint has been spammed)"
            );
        }
        e => panic!("{e:?}"),
    }
}

#[tokio::test]
async fn secondary_rate_limit_on_a_write_is_not_retried() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("retry-after", "1")
                .set_body_json(json!({"message": "You have exceeded a secondary rate limit"})),
        )
        .expect(1)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1347, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::RateLimited);
}

#[tokio::test]
async fn writes_follow_no_redirect() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo-org/api/issues/1347/comments"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/moved/comments", s.uri()).as_str()),
        )
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(any())
        .and(path("/moved/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(0)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1347, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Http(307));
}

#[tokio::test]
async fn writes_are_spaced() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo-org/api/issues/1347/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 5, "body": "x", "user": {"login": "octocat", "id": 1},
            "created_at": "2026-11-14T10:00:00Z", "updated_at": "2026-11-14T10:00:00Z"
        })))
        .expect(3)
        .mount(&s)
        .await;
    let p = provider(&s);
    let c = NewComment::Pr { text: "x".into() };
    let start = Instant::now();
    p.post_comment(&repo(), 1347, &at(), &c).await.unwrap();
    // the first write does not wait
    assert!(start.elapsed() < MIN_WRITE_SPACING);
    p.post_comment(&repo(), 1347, &at(), &c).await.unwrap();
    p.post_comment(&repo(), 1347, &at(), &c).await.unwrap();
    assert!(MIN_WRITE_SPACING >= Duration::from_secs(1));
    assert!(
        start.elapsed() >= 2 * MIN_WRITE_SPACING,
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn token_never_in_write_errors() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(TOKEN))
        .mount(&s)
        .await;
    let p = provider(&s);
    let err = p
        .post_comment(&repo(), 1347, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    let shown = format!("{err} {err:?} {} {p:?}", err.user_message());
    assert!(!shown.contains(TOKEN), "{shown}");
}

// ---- opt-in live write (never run by default) ----

/// The live write guard: `confirm` must be exactly `<owner>/<repo>#<n>` of the target.
fn live_confirm_ok(confirm: Option<&str>, repo: &str, pr: u64) -> bool {
    confirm == Some(format!("{repo}#{pr}").as_str())
}

#[test]
fn live_confirm_must_match_the_target_exactly() {
    assert!(live_confirm_ok(Some("o/r#7"), "o/r", 7));
    for bad in ["yes", "o/r#8", "O/r#7", " o/r#7", "o/r#7 ", "o/r", ""] {
        assert!(!live_confirm_ok(Some(bad), "o/r", 7), "{bad}");
    }
    assert!(!live_confirm_ok(None, "o/r", 7));
}

/// Writes to a real THROWAWAY PR on github.com: a PR comment, a file comment on the PR's
/// first changed file, a reply to it, then resolve and unresolve of its thread. Reviews are
/// not sent. Run only against a repository and PR you can delete:
///
/// ```text
/// $env:NIT_LIVE_GITHUB_TOKEN = Read-Host -MaskInput "GitHub token"
/// $env:NIT_LIVE_GITHUB_REPO = "<owner>/<repo>"
/// $env:NIT_LIVE_GITHUB_PR = "<n>"
/// $env:NIT_LIVE_CONFIRM_THROWAWAY = "<owner>/<repo>#<n>"
/// cargo test -p providers --test github_write -- --ignored --nocapture
/// ```
///
/// The token is read from the environment only and never printed.
#[tokio::test]
#[ignore]
async fn live_write_comments_reply_and_resolve_on_a_throwaway_pr() {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (Some(token), Some(full), Some(pr)) = (
        env("NIT_LIVE_GITHUB_TOKEN"),
        env("NIT_LIVE_GITHUB_REPO"),
        env("NIT_LIVE_GITHUB_PR").and_then(|n| n.parse::<u64>().ok()),
    ) else {
        eprintln!("skipped: set NIT_LIVE_GITHUB_TOKEN, NIT_LIVE_GITHUB_REPO, NIT_LIVE_GITHUB_PR");
        return;
    };
    assert!(
        live_confirm_ok(env("NIT_LIVE_CONFIRM_THROWAWAY").as_deref(), &full, pr),
        "refusing to write: NIT_LIVE_CONFIRM_THROWAWAY must be exactly {full}#{pr}"
    );
    let repo = RepoRef::parse("github.com", &full).expect("<owner>/<repo>");
    let p = GitHubProvider::new(
        "https://api.github.com",
        "https://api.github.com/graphql",
        "github.com",
        &token,
    );
    let info = p.pr(&repo, pr).await.expect("GET /pulls/{n}");
    eprintln!(
        "target: {full}#{pr} \"{}\" by {} ({:?}), {} -> {}",
        info.title, info.author.display_name, info.state, info.source_branch, info.dest_branch
    );
    let at = PostRevision {
        head: info.source_commit.clone(),
        merge_base: String::new(),
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let c = p
        .post_comment(
            &repo,
            pr,
            &at,
            &NewComment::Pr {
                text: format!("nit live write test {stamp}: PR comment"),
            },
        )
        .await
        .expect("PR comment");
    eprintln!("PR comment {}", c.id);
    let files = p
        .client()
        .get_json(&format!("/repos/{full}/pulls/{pr}/files?per_page=1"))
        .await
        .expect("files");
    let file = files[0]["filename"].as_str().expect("a changed file");
    let fc = p
        .post_comment(
            &repo,
            pr,
            &at,
            &NewComment::File {
                path: file.into(),
                text: format!("nit live write test {stamp}: file comment"),
            },
        )
        .await
        .expect("file comment");
    eprintln!("file comment {} on {file}", fc.id);
    let list = p.comments(&repo, pr).await.expect("comments");
    let thread = list
        .comments
        .iter()
        .find(|x| x.id == fc.id)
        .and_then(|x| x.thread.clone())
        .expect("the file comment's thread");
    eprintln!("thread {thread}");
    let r = p
        .post_comment(
            &repo,
            pr,
            &at,
            &NewComment::Reply {
                thread: thread.clone(),
                text: format!("nit live write test {stamp}: reply"),
            },
        )
        .await
        .expect("reply");
    eprintln!("reply {}", r.id);
    p.set_resolved(&repo, pr, &thread, true)
        .await
        .expect("resolve");
    p.set_resolved(&repo, pr, &thread, false)
        .await
        .expect("unresolve");
    // optional: NIT_LIVE_GITHUB_INLINE="<path>:<new-file line>" posts one inline comment
    if let Some((path, line)) = env("NIT_LIVE_GITHUB_INLINE")
        .and_then(|v| {
            v.rsplit_once(':')
                .map(|(a, b)| (a.to_string(), b.to_string()))
        })
        .and_then(|(a, b)| b.parse::<u32>().ok().map(|n| (a, n)))
    {
        let ic = p
            .post_comment(
                &repo,
                pr,
                &at,
                &NewComment::Inline {
                    path: path.clone(),
                    side: Side::New,
                    line,
                    start: None,
                    text: format!("nit live write test {stamp}: inline"),
                },
            )
            .await
            .expect("inline comment");
        eprintln!("inline comment {} on {path}:{line}", ic.id);
    }
    eprintln!("resolved and unresolved; check the PR in the web UI");
}
