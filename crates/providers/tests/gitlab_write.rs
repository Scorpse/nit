//! GitLabProvider writes against a fake GitLab (wiremock): one exact request body per
//! mapping (plan Task 27), refusals before any request, lost answers, 4xx, 429, spacing.
//! Diff versions and discussion echoes are live-recorded (GitLab 19.5.1, see SOURCES.md).
//!
//! The `live_*` test at the bottom writes to a real throwaway MR and is never run by default
//! (`#[ignore]`, see its doc comment).

use std::time::{Duration, Instant};

use prcore::comments::{NewComment, ReviewAction, ThreadId};
use prcore::draft::Side;
use prcore::model::RepoRef;
use providers::gitlab::position::sha1_hex;
use providers::gitlab::{bases_for, GitLabProvider, MIN_WRITE_SPACING, REQUEST_CHANGES};
use providers::provider::{set_review_checked, PostRevision, Provider};
use providers::ProviderError;
use serde_json::{json, Value};
use wiremock::matchers::{any, body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "glpat-test-T0KEN-not-real";
const HOST: &str = "gitlab.example";
const MR: &str = "/api/v4/projects/root%2Fnit-live-test/merge_requests/1";
/// The first diff version of the live throwaway MR (`mr_version_live_861.json`).
const HEAD: &str = "5536c4a86d07bdb808b71ea045bde72d9f4ae3eb";
const BASE: &str = "72628a0ed18c99c54b52663a7d0e417a1da8df63";
const DISCUSSION: &str = "0e189cd1944eef35662e70e9ec73f04eb12cd55a";

fn provider(s: &MockServer) -> GitLabProvider {
    provider_at(&s.uri())
}

fn provider_at(base: &str) -> GitLabProvider {
    GitLabProvider::new(&format!("{base}/api/v4"), HOST, TOKEN)
}

fn repo() -> RepoRef {
    RepoRef::parse(HOST, "root/nit-live-test").unwrap()
}

fn at() -> PostRevision {
    PostRevision {
        head: HEAD.into(),
        merge_base: BASE.into(),
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/gitlab/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn ok(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(fixture(name), "application/json")
}

/// A note as `POST .../notes` echoes it.
fn note_echo(id: u64, body: &str) -> Value {
    json!({
        "id": id, "type": null, "body": body, "system": false,
        "author": {"id": 2, "username": "root", "name": "Administrator"},
        "created_at": "2026-11-15T02:22:17.682Z", "updated_at": "2026-11-15T02:22:17.682Z",
        "resolvable": false, "resolved": false, "noteable_iid": 1
    })
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

/// The diff versions (live) and version 861 with its diffs; no write is mounted.
async fn mount_versions(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("{MR}/versions")))
        .respond_with(ok("mr_versions_live"))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR}/versions/861")))
        .respond_with(ok("mr_version_live_861"))
        .mount(s)
        .await;
}

fn text_position(lines: Value) -> Value {
    let mut p = json!({
        "base_sha": BASE, "start_sha": BASE, "head_sha": HEAD, "position_type": "text",
        "new_path": "small.txt", "old_path": "small.txt"
    });
    for (k, v) in lines.as_object().unwrap() {
        p[k] = v.clone();
    }
    p
}

/// Posts `c` expecting exactly `body` at `.../discussions`; answers with the live echo.
async fn post_discussion(c: NewComment, body: Value) -> prcore::comments::Comment {
    let s = MockServer::start().await;
    mount_versions(&s).await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/discussions")))
        .and(body_json(body))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_raw(fixture("discussion_live_added"), "application/json"),
        )
        .expect(1)
        .mount(&s)
        .await;
    provider(&s)
        .post_comment(&repo(), 1, &at(), &c)
        .await
        .unwrap()
}

fn inline(path: &str, side: Side, line: u32, start: Option<(Side, u32)>) -> NewComment {
    NewComment::Inline {
        path: path.into(),
        side,
        line,
        start,
        text: "nit".into(),
    }
}

// ---- comments ----

#[tokio::test]
async fn mr_comment_posts_a_note() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/notes")))
        .and(body_json(json!({"body": "Looks good"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(note_echo(760, "Looks good")))
        .expect(1)
        .mount(&s)
        .await;
    let c = provider(&s)
        .post_comment(
            &repo(),
            1,
            &at(),
            &NewComment::Pr {
                text: "Looks good".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(c.id.as_str(), "note:760");
    // the note's discussion id is not in the answer: known after the next read
    assert_eq!((c.thread, c.anchor), (None, None));
    assert_eq!((c.body.as_str(), c.author.id.as_str()), ("Looks good", "2"));
}

#[tokio::test]
async fn file_comment_posts_a_file_position_on_the_reviewed_version() {
    let c = post_discussion(
        NewComment::File {
            path: "new-name.txt".into(),
            text: "whole file".into(),
        },
        json!({"body": "whole file", "position": {
            "base_sha": BASE, "start_sha": BASE, "head_sha": HEAD, "position_type": "file",
            "new_path": "new-name.txt", "old_path": "old-name.txt"
        }}),
    )
    .await;
    // the echoed discussion's first note, joined to it
    assert_eq!(c.id.as_str(), "note:751");
    assert_eq!(c.thread.as_ref().map(|t| t.as_str()), Some(DISCUSSION));
    assert!(c.can_resolve && !c.resolved);
}

#[tokio::test]
async fn added_line_sends_new_line_only() {
    post_discussion(
        inline("small.txt", Side::New, 5, None),
        json!({"body": "nit", "position": text_position(json!({"new_line": 5}))}),
    )
    .await;
}

#[tokio::test]
async fn deleted_line_sends_old_line_only() {
    post_discussion(
        inline("small.txt", Side::Old, 3, None),
        json!({"body": "nit", "position": text_position(json!({"old_line": 3}))}),
    )
    .await;
}

#[tokio::test]
async fn context_sends_both() {
    // small.txt new 3 is old 4 ("line 4": line 3 was removed above it)
    post_discussion(
        inline("small.txt", Side::New, 3, None),
        json!({"body": "nit", "position": text_position(json!({"old_line": 4, "new_line": 3}))}),
    )
    .await;
}

#[tokio::test]
async fn far_hunk_and_rename_keep_their_paths() {
    let mut far = text_position(json!({"new_line": 2200}));
    far["new_path"] = "big.txt".into();
    far["old_path"] = "big.txt".into();
    post_discussion(
        inline("big.txt", Side::New, 2200, None),
        json!({"body": "nit", "position": far}),
    )
    .await;
    let mut renamed = text_position(json!({"new_line": 3}));
    renamed["new_path"] = "new-name.txt".into();
    renamed["old_path"] = "old-name.txt".into();
    post_discussion(
        inline("new-name.txt", Side::New, 3, None),
        json!({"body": "nit", "position": renamed}),
    )
    .await;
}

#[tokio::test]
async fn multi_line_sends_a_line_range_with_line_codes() {
    let code = sha1_hex(b"small.txt");
    // GitLab's own line code for small.txt (from its "changed this line" system note)
    assert_eq!(code, "7dce724251cb396cebf1b3ea068a13023968315f");
    let mut p = text_position(json!({"new_line": 8}));
    p["line_range"] = json!({
        "start": {"line_code": format!("{code}_6_6"), "type": null, "old_line": 6, "new_line": 6},
        "end": {"line_code": format!("{code}_9_8"), "type": "new", "old_line": null, "new_line": 8},
    });
    post_discussion(
        inline("small.txt", Side::New, 8, Some((Side::New, 6))),
        json!({"body": "nit", "position": p}),
    )
    .await;
}

/// Lines GitLab answers 400 for (live): outside every hunk, past the end, a context line on
/// the wrong side. nit refuses them before any write.
#[tokio::test]
async fn line_outside_the_diff_is_invalid_before_any_write() {
    let s = MockServer::start().await;
    mount_versions(&s).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&s)
        .await;
    let p = provider(&s);
    for c in [
        inline("big.txt", Side::New, 100, None),
        inline("small.txt", Side::New, 50, None),
        inline("small.txt", Side::Old, 40, None),
        inline("small.txt", Side::New, 8, Some((Side::New, 70))),
        inline("not-in-the-diff.txt", Side::New, 1, None),
        NewComment::File {
            path: "not-in-the-diff.txt".into(),
            text: "x".into(),
        },
    ] {
        let err = p.post_comment(&repo(), 1, &at(), &c).await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{c:?}: {err:?}");
    }
}

#[tokio::test]
async fn unknown_head_or_moved_base_is_invalid_before_any_write() {
    let s = MockServer::start().await;
    mount_versions(&s).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&s)
        .await;
    let p = provider(&s);
    let c = inline("small.txt", Side::New, 5, None);
    let unknown = PostRevision {
        head: "1111111111111111111111111111111111111111".into(),
        merge_base: BASE.into(),
    };
    let moved = PostRevision {
        head: HEAD.into(),
        merge_base: "2222222222222222222222222222222222222222".into(),
    };
    for at in [unknown, moved, PostRevision::default()] {
        let err = p.post_comment(&repo(), 1, &at, &c).await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{at:?}: {err:?}");
    }
}

#[tokio::test]
async fn reply_posts_a_note_to_the_discussion() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/discussions/{DISCUSSION}/notes")))
        .and(body_json(json!({"body": "done"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(note_echo(761, "done")))
        .expect(1)
        .mount(&s)
        .await;
    let thread = ThreadId(DISCUSSION.into());
    let c = provider(&s)
        .post_comment(
            &repo(),
            1,
            &at(),
            &NewComment::Reply {
                thread: thread.clone(),
                text: "done".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(c.id.as_str(), "note:761");
    assert_eq!(c.thread, Some(thread));
}

#[tokio::test]
async fn bad_discussion_id_is_invalid_before_any_request() {
    let s = no_requests().await;
    for t in ["", "abc/../x", "abc?resolved=false"] {
        let p = provider(&s);
        let reply = NewComment::Reply {
            thread: ThreadId(t.into()),
            text: "x".into(),
        };
        let e1 = p.post_comment(&repo(), 1, &at(), &reply).await.unwrap_err();
        let e2 = p
            .set_resolved(&repo(), 1, &ThreadId(t.into()), true)
            .await
            .unwrap_err();
        assert!(matches!(e1, ProviderError::Invalid(_)), "{t}: {e1:?}");
        assert!(matches!(e2, ProviderError::Invalid(_)), "{t}: {e2:?}");
    }
}

#[tokio::test]
async fn slash_command_refused_before_request() {
    let s = no_requests().await;
    let p = provider(&s);
    for c in [
        NewComment::Pr {
            text: "/approve".into(),
        },
        NewComment::Reply {
            thread: ThreadId(DISCUSSION.into()),
            text: "fine\n  /merge".into(),
        },
        NewComment::Inline {
            path: "small.txt".into(),
            side: Side::New,
            line: 5,
            start: None,
            text: "/assign @me".into(),
        },
    ] {
        let err = p.post_comment(&repo(), 1, &at(), &c).await.unwrap_err();
        assert_eq!(
            err,
            ProviderError::Invalid(providers::gitlab::QUICK_ACTION.into())
        );
    }
}

// ---- resolve ----

#[tokio::test]
async fn resolve_and_unresolve_set_the_discussion_flag() {
    let s = MockServer::start().await;
    for flag in ["true", "false"] {
        Mock::given(method("PUT"))
            .and(path(format!("{MR}/discussions/{DISCUSSION}")))
            .and(query_param("resolved", flag))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": DISCUSSION, "notes": []})),
            )
            .expect(1)
            .mount(&s)
            .await;
    }
    let p = provider(&s);
    let t = ThreadId(DISCUSSION.into());
    p.set_resolved(&repo(), 1, &t, true).await.unwrap();
    p.set_resolved(&repo(), 1, &t, false).await.unwrap();
}

// ---- reviews ----

/// `GET` of the MR (`mr_detail.json`: head `33e2ee85...`).
async fn mount_mr(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path(MR))
        .respond_with(ok("mr_detail"))
        .mount(s)
        .await;
}

const DETAIL_HEAD: &str = "33e2ee8579fda5bc36accc9c6fbd0b4fefda9e30";

#[tokio::test]
async fn approve_sends_the_head_as_sha() {
    let s = MockServer::start().await;
    mount_mr(&s).await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/approve")))
        .and(body_json(json!({"sha": DETAIL_HEAD})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"user_has_approved": true})))
        .expect(1)
        .mount(&s)
        .await;
    set_review_checked(
        &provider(&s),
        &repo(),
        1,
        ReviewAction::Approve,
        Some("ignored"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn approve_409_is_rejected_with_reload_hint() {
    let s = MockServer::start().await;
    mount_mr(&s).await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/approve")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "message": "SHA does not match HEAD of source branch: cf2df1b74e3d125fc42d6bbd8a4dad1d19ab5c85"
        })))
        .expect(1)
        .mount(&s)
        .await;
    let err = set_review_checked(&provider(&s), &repo(), 1, ReviewAction::Approve, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProviderError::Rejected { status: 409, message } if message.contains("reload")),
        "{err:?}"
    );
}

/// Live: approving again answers 401 (the token is fine: the MR was just read with it).
#[tokio::test]
async fn approve_401_is_a_refusal_not_a_bad_token() {
    let s = MockServer::start().await;
    mount_mr(&s).await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/approve")))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "401 Unauthorized"})),
        )
        .expect(1)
        .mount(&s)
        .await;
    let err = set_review_checked(&provider(&s), &repo(), 1, ReviewAction::Approve, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProviderError::Rejected { status: 401, message } if message.contains("browser")),
        "{err:?}"
    );
}

#[tokio::test]
async fn unapprove_and_its_404() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/unapprove")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"user_has_approved": false})))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/unapprove")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "404 Not Found"})))
        .mount(&s)
        .await;
    let p = provider(&s);
    set_review_checked(&p, &repo(), 1, ReviewAction::ClearApproval, None)
        .await
        .unwrap();
    let err = set_review_checked(&p, &repo(), 1, ReviewAction::ClearApproval, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProviderError::Rejected { status: 404, message } if message.contains("no approval")),
        "{err:?}"
    );
}

#[tokio::test]
async fn request_changes_uses_the_graphql_mutation() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .and(body_json(json!({
            "query": REQUEST_CHANGES,
            "variables": {"projectPath": "root/nit-live-test", "iid": "1"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"mergeRequestRequestChanges": {"errors": []}}
        })))
        .expect(1)
        .mount(&s)
        .await;
    // GitLab takes no text with it: the body is not sent
    set_review_checked(
        &provider(&s),
        &repo(),
        1,
        ReviewAction::RequestChanges,
        Some("x"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn request_changes_errors_are_rejected() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"mergeRequestRequestChanges": {"errors": ["You do not have permission"]}}
        })))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"mergeRequestRequestChanges": null},
            "errors": [{"message": "The resource that you are attempting to access does not exist"}]
        })))
        .mount(&s)
        .await;
    let p = provider(&s);
    for want in ["permission", "does not exist"] {
        let err = set_review_checked(&p, &repo(), 1, ReviewAction::RequestChanges, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ProviderError::Rejected { message, .. } if message.contains(want)),
            "{err:?}"
        );
    }
}

#[tokio::test]
async fn clear_request_changes_is_invalid_before_any_request() {
    let s = no_requests().await;
    let p = provider(&s);
    assert!(!p.capabilities().allows(ReviewAction::ClearRequestChanges));
    let err = p
        .set_review(&repo(), 1, ReviewAction::ClearRequestChanges, None)
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");
}

#[test]
fn gitlab_reports_its_write_capabilities() {
    let p = GitLabProvider::new("https://gitlab.com/api/v4", "gitlab.com", TOKEN);
    let caps = p.capabilities();
    assert!(!caps.read_only);
    assert!(caps.allows(ReviewAction::Approve) && caps.allows(ReviewAction::ClearApproval));
    assert!(caps.allows(ReviewAction::RequestChanges));
    assert!(!caps.allows(ReviewAction::ClearRequestChanges));
    assert!(caps.resolve_threads && caps.file_comments && caps.reply_to_general_comments);
    // inline posting was enabled after the live anchoring run
    const _: () = assert!(providers::gitlab::INLINE_ENABLED_GITLAB);
    assert_eq!(p.client().graphql_url(), "https://gitlab.com/api/graphql");
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
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Unconfirmed);
    let base = dropping_server().await;
    let err = provider_at(&base)
        .set_resolved(&repo(), 1, &ThreadId(DISCUSSION.into()), true)
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
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
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
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::PostedUnreadable);
}

/// The live 400 for a position GitLab cannot place, and the object-shaped message.
#[tokio::test]
async fn validation_400_is_rejected_with_gitlabs_message() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/notes")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "400 Bad request - Note {:line_code=>[\"can't be blank\", \"must be a valid line code\"]}"
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/notes")))
        .respond_with(ResponseTemplate::new(422).set_body_json(
            json!({"message": {"note": ["is too long (maximum is 1000000 characters)"]}}),
        ))
        .mount(&s)
        .await;
    let p = provider(&s);
    let c = NewComment::Pr { text: "x".into() };
    match p.post_comment(&repo(), 1, &at(), &c).await.unwrap_err() {
        ProviderError::Rejected { status, message } => {
            assert_eq!(status, 400);
            assert!(message.contains("must be a valid line code"), "{message}");
        }
        e => panic!("{e:?}"),
    }
    match p.post_comment(&repo(), 1, &at(), &c).await.unwrap_err() {
        ProviderError::Rejected { status, message } => {
            assert_eq!(status, 422);
            assert_eq!(message, "note is too long (maximum is 1000000 characters)");
        }
        e => panic!("{e:?}"),
    }
}

#[tokio::test]
async fn rate_limited_429_pauses_and_is_not_retried() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .expect(1)
        .mount(&s)
        .await;
    let p = provider(&s);
    let c = NewComment::Pr { text: "x".into() };
    assert_eq!(
        p.post_comment(&repo(), 1, &at(), &c).await.unwrap_err(),
        ProviderError::RateLimited
    );
    assert!(p.paused_until().is_some());
    // paused: the next write is refused without a request
    assert_eq!(
        p.post_comment(&repo(), 1, &at(), &c).await.unwrap_err(),
        ProviderError::RateLimited
    );
}

#[tokio::test]
async fn writes_follow_no_redirect() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/notes")))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/moved/notes", s.uri()).as_str()),
        )
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(any())
        .and(path("/moved/notes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(note_echo(1, "x")))
        .expect(0)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Http(307));
}

#[tokio::test]
async fn writes_are_spaced() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{MR}/notes")))
        .respond_with(ResponseTemplate::new(201).set_body_json(note_echo(5, "x")))
        .expect(3)
        .mount(&s)
        .await;
    let p = provider(&s);
    let c = NewComment::Pr { text: "x".into() };
    let start = Instant::now();
    p.post_comment(&repo(), 1, &at(), &c).await.unwrap();
    assert!(start.elapsed() < MIN_WRITE_SPACING);
    p.post_comment(&repo(), 1, &at(), &c).await.unwrap();
    p.post_comment(&repo(), 1, &at(), &c).await.unwrap();
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
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"message": "x"})))
        .mount(&s)
        .await;
    let p = provider(&s);
    let err = p
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    let shown = format!("{err} {err:?} {} {p:?}", err.user_message());
    assert!(!shown.contains(TOKEN), "{shown}");
}

// ---- opt-in live write (never run by default) ----

/// The live write guard: `confirm` must be exactly `<group/project>!<iid>` of the target.
fn live_confirm_ok(confirm: Option<&str>, project: &str, mr: u64) -> bool {
    confirm == Some(format!("{project}!{mr}").as_str())
}

#[test]
fn live_confirm_must_match_the_target_exactly() {
    assert!(live_confirm_ok(Some("g/p!7"), "g/p", 7));
    for bad in [
        "yes", "g/p!8", "G/p!7", " g/p!7", "g/p!7 ", "g/p#7", "g/p", "",
    ] {
        assert!(!live_confirm_ok(Some(bad), "g/p", 7), "{bad}");
    }
    assert!(!live_confirm_ok(None, "g/p", 7));
}

/// Writes to a real THROWAWAY MR: an MR note, a file comment and an inline comment (an added
/// line, a context line, a removed line and a multi-line range, all looked up in GitLab's
/// diff of the MR's head), a reply, resolve and unresolve, approve, unapprove, and request
/// changes. Every write is read back with `comments()`. Run only against a project and MR
/// you can throw away:
///
/// ```text
/// $env:NIT_LIVE_GITLAB_URL = "https://gitlab.example"
/// $env:NIT_LIVE_GITLAB_TOKEN = Read-Host -MaskInput "GitLab token (api scope)"
/// $env:NIT_LIVE_GITLAB_PROJECT = "<group/project>"
/// $env:NIT_LIVE_GITLAB_MR = "<iid>"
/// $env:NIT_LIVE_CONFIRM_THROWAWAY = "<group/project>!<iid>"
/// cargo test -p providers --test gitlab_write -- --ignored --nocapture
/// ```
///
/// The token is read from the environment only and never printed.
#[tokio::test]
#[ignore]
async fn live_write_comments_reply_resolve_and_reviews_on_a_throwaway_mr() {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (Some(url), Some(token), Some(project), Some(mr)) = (
        env("NIT_LIVE_GITLAB_URL"),
        env("NIT_LIVE_GITLAB_TOKEN"),
        env("NIT_LIVE_GITLAB_PROJECT"),
        env("NIT_LIVE_GITLAB_MR").and_then(|n| n.parse::<u64>().ok()),
    ) else {
        eprintln!("skipped: set NIT_LIVE_GITLAB_URL, _TOKEN, _PROJECT and _MR");
        return;
    };
    assert!(
        live_confirm_ok(env("NIT_LIVE_CONFIRM_THROWAWAY").as_deref(), &project, mr),
        "refusing to write: NIT_LIVE_CONFIRM_THROWAWAY must be exactly {project}!{mr}"
    );
    let b = bases_for(&url).expect("NIT_LIVE_GITLAB_URL");
    let repo = RepoRef::parse(&b.git_host, &project).expect("<group>/<project>");
    let p = GitLabProvider::new(&b.api_base, &b.git_host, &token);
    let info = p.pr(&repo, mr).await.expect("GET MR");
    eprintln!(
        "target: {project}!{mr} \"{}\" by {} ({:?}), {} -> {}",
        info.title, info.author.display_name, info.state, info.source_branch, info.dest_branch
    );
    let rev = p.revision(&repo, mr).await.expect("revision");
    let at = PostRevision {
        head: rev.head.clone(),
        merge_base: rev.base.clone().unwrap_or_default(),
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let say = |what: &str| format!("nit live write test {stamp}: {what}");

    let note = p
        .post_comment(
            &repo,
            mr,
            &at,
            &NewComment::Pr {
                text: say("MR note"),
            },
        )
        .await
        .expect("MR note");
    eprintln!("MR note {}", note.id);

    // the first file of the head's diff, and its lines per kind
    let versions = p
        .client()
        .get_json(&format!(
            "/projects/{}/merge_requests/{mr}/versions",
            project.replace('/', "%2F")
        ))
        .await
        .expect("versions");
    let vid = versions
        .as_array()
        .and_then(|l| l.iter().find(|v| v["head_commit_sha"] == at.head.as_str()))
        .and_then(|v| v["id"].as_u64())
        .expect("a version of the head");
    let version = p
        .client()
        .get_json(&format!(
            "/projects/{}/merge_requests/{mr}/versions/{vid}",
            project.replace('/', "%2F")
        ))
        .await
        .expect("version");
    let (file, lines) = version["diffs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            (
                d["new_path"].as_str().unwrap_or_default().to_string(),
                providers::gitlab::position::diff_lines(d["diff"].as_str().unwrap_or_default()),
            )
        })
        .max_by_key(|(_, l)| l.len())
        .expect("a changed file");
    eprintln!("file {file}: {} diff lines", lines.len());

    let fc = p
        .post_comment(
            &repo,
            mr,
            &at,
            &NewComment::File {
                path: file.clone(),
                text: say("file comment"),
            },
        )
        .await
        .expect("file comment");
    let thread = fc.thread.clone().expect("the file comment's discussion");
    eprintln!("file comment {} in {thread}", fc.id);

    use providers::gitlab::position::LineKind;
    let mut posted = Vec::new();
    for kind in [LineKind::Added, LineKind::Context, LineKind::Removed] {
        let Some(l) = lines.iter().find(|l| l.kind == kind) else {
            eprintln!("no {kind:?} line in {file}");
            continue;
        };
        let (side, n) = match kind {
            LineKind::Removed => (Side::Old, l.old),
            _ => (Side::New, l.new),
        };
        let c = p
            .post_comment(
                &repo,
                mr,
                &at,
                &NewComment::Inline {
                    path: file.clone(),
                    side,
                    line: n,
                    start: None,
                    text: say(&format!("inline {kind:?} {side:?} {n}")),
                },
            )
            .await
            .unwrap_or_else(|e| panic!("inline {kind:?}: {}", e.user_message()));
        let a = c.anchor.clone().expect("an anchor");
        eprintln!(
            "inline {kind:?} {side:?} {n} -> {} from {:?} to {:?}",
            c.id, a.from, a.to
        );
        posted.push((c.id.clone(), side, n));
    }
    let added: Vec<_> = lines
        .iter()
        .filter(|l| l.kind != LineKind::Removed)
        .collect();
    if added.len() >= 2 {
        let (s, e) = (added[0].new, added[added.len() - 1].new);
        let c = p
            .post_comment(
                &repo,
                mr,
                &at,
                &NewComment::Inline {
                    path: file.clone(),
                    side: Side::New,
                    line: e,
                    start: Some((Side::New, s)),
                    text: say(&format!("multi-line new {s}..{e}")),
                },
            )
            .await
            .expect("multi-line");
        let a = c.anchor.clone().unwrap();
        eprintln!(
            "multi-line {s}..{e} -> {} start {:?} to {:?}",
            c.id, a.start, a.to
        );
        assert_eq!((a.start, a.to), (Some((Side::New, s)), Some(e)));
    }

    let r = p
        .post_comment(
            &repo,
            mr,
            &at,
            &NewComment::Reply {
                thread: thread.clone(),
                text: say("reply"),
            },
        )
        .await
        .expect("reply");
    eprintln!("reply {}", r.id);
    p.set_resolved(&repo, mr, &thread, true)
        .await
        .expect("resolve");
    let read = p.comments(&repo, mr).await.expect("comments");
    let resolved = read
        .comments
        .iter()
        .filter(|c| c.thread.as_ref() == Some(&thread));
    assert!(resolved.clone().count() >= 2 && resolved.clone().all(|c| c.resolved));
    p.set_resolved(&repo, mr, &thread, false)
        .await
        .expect("unresolve");

    let read = p.comments(&repo, mr).await.expect("comments");
    assert!(read.skipped.is_empty(), "{:?}", read.skipped);
    for (id, side, n) in &posted {
        let c = read
            .comments
            .iter()
            .find(|c| &c.id == id)
            .expect("read back");
        let a = c.anchor.as_ref().unwrap();
        let got = match side {
            Side::Old => a.from,
            Side::New => a.to,
        };
        assert_eq!(got, Some(*n), "{id}");
        assert!(!a.outdated, "{id}");
    }
    assert!(read.comments.iter().any(|c| c.body == say("MR note")));
    assert!(read
        .comments
        .iter()
        .filter(|c| c.thread.as_ref() == Some(&thread))
        .all(|c| !c.resolved));
    eprintln!(
        "read back {} notes: positions and resolved flags as posted",
        read.comments.len()
    );

    set_review_checked(&p, &repo, mr, ReviewAction::Approve, None)
        .await
        .expect("approve");
    eprintln!("approved");
    set_review_checked(&p, &repo, mr, ReviewAction::ClearApproval, None)
        .await
        .expect("unapprove");
    eprintln!("unapproved");
    set_review_checked(&p, &repo, mr, ReviewAction::RequestChanges, None)
        .await
        .expect("request changes");
    eprintln!("requested changes; check the MR in the web UI");
}
