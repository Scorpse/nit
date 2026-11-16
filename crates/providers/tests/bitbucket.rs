use chrono::{TimeZone, Utc};
use prcore::comments::{CommentId, ThreadId};
use prcore::model::{PrState, RepoInfo, RepoRef, ReviewState, UserRef};
use providers::bitbucket::BitbucketProvider;
use providers::provider::PostRevision;
use providers::provider::{PrList, Provider, SkippedPr};
use providers::BbClient;
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider(server: &MockServer) -> BitbucketProvider {
    BitbucketProvider::new(BbClient::new(&server.uri(), "me@example.com", "tok"))
}

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

fn repo() -> RepoRef {
    RepoRef::parse_bitbucket("acme/api").unwrap()
}

async fn mount_json(server: &MockServer, p: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn user(uuid: &str, name: &str) -> UserRef {
    UserRef {
        id: uuid.into(),
        display_name: name.into(),
        login: None,
    }
}

const PRS_PATH: &str = "/repositories/acme/api/pullrequests";

#[tokio::test]
async fn me_maps_uuid_and_display_name() {
    let s = MockServer::start().await;
    mount_json(&s, "/user", fixture(include_str!("fixtures/me.json"))).await;
    let me = provider(&s).me().await.unwrap();
    assert_eq!(
        me,
        user("{11111111-1111-1111-1111-111111111111}", "Ada Lovelace")
    );
}

#[tokio::test]
async fn workspaces_extracts_slugs() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        "/user/workspaces",
        fixture(include_str!("fixtures/workspaces.json")),
    )
    .await;
    assert_eq!(
        provider(&s).workspaces().await.unwrap(),
        vec!["acme".to_string(), "side-project".to_string()]
    );
}

#[tokio::test]
async fn repos_parses_full_names_and_skips_unparseable() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repositories/acme"))
        .and(query_param("role", "member"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture(include_str!("fixtures/repos.json"))),
        )
        .expect(1)
        .mount(&s)
        .await;
    let repos: Vec<RepoRef> = provider(&s)
        .repo_infos("acme")
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.repo)
        .collect();
    assert_eq!(
        repos,
        vec![
            RepoRef::parse_bitbucket("acme/api").unwrap(),
            RepoRef::parse_bitbucket("acme/web").unwrap()
        ]
    );
}

#[tokio::test]
async fn repo_infos_carry_name_and_visibility_when_given() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repositories/acme"))
        .and(query_param("role", "member"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture(include_str!("fixtures/repos.json"))),
        )
        .expect(1)
        .mount(&s)
        .await;
    let infos = provider(&s).repo_infos("acme").await.unwrap();
    assert_eq!(
        infos,
        vec![
            RepoInfo {
                repo: RepoRef::parse_bitbucket("acme/api").unwrap(),
                name: Some("API Service".into()),
                private: Some(true),
            },
            RepoInfo {
                repo: RepoRef::parse_bitbucket("acme/web").unwrap(),
                name: Some("web".into()),
                private: None,
            },
        ]
    );
}

#[tokio::test]
async fn prs_open_maps_every_field() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PRS_PATH))
        .and(query_param("state", "OPEN"))
        .and(query_param("pagelen", "50"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(fixture(include_str!("fixtures/prs_open.json"))),
        )
        .expect(1)
        .mount(&s)
        .await;
    let prs = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap()
        .prs;
    assert_eq!(prs.len(), 2);
    let pr = &prs[0];
    assert_eq!(pr.repo, repo());
    assert_eq!(pr.id, 42);
    assert_eq!(pr.title, "Add retry logic");
    assert_eq!(pr.state, PrState::Open);
    assert!(!pr.draft);
    assert_eq!(pr.author, user("{aaaa}", "Alice Author"));
    assert_eq!(pr.source_branch, "feature/retry");
    assert_eq!(pr.dest_branch, "main");
    assert_eq!(pr.source_commit, "abc123def456");
    assert_eq!(pr.dest_commit, "fff000eee111");
    assert_eq!(pr.source_repo, repo());
    assert!(!pr.is_fork());
    assert_eq!(pr.comment_count, 7);
    assert_eq!(pr.task_count, 2);
    let got: Vec<(UserRef, ReviewState)> = pr
        .reviewers
        .iter()
        .map(|r| (r.user.clone(), r.state.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (user("{bbbb}", "Bob Approver"), ReviewState::Approved),
            (
                user("{cccc}", "Carol Changes"),
                ReviewState::ChangesRequested
            ),
            (user("{dddd}", "Dan Pending"), ReviewState::Pending),
            (user("{eeee}", "Eve Approved Flag"), ReviewState::Approved),
            (
                user("{gggg}", "Grace Unlisted Approver"),
                ReviewState::Approved
            ),
        ]
    );
    assert_eq!(
        pr.created_on,
        Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap()
    );
    assert_eq!(
        pr.updated_on,
        Utc.with_ymd_and_hms(2026, 3, 2, 11, 30, 15).unwrap() + chrono::Duration::milliseconds(250)
    );
    // Second PR: draft, missing counts and missing destination repository.
    let draft = &prs[1];
    assert!(draft.draft);
    assert_eq!(draft.comment_count, 0);
    assert_eq!(draft.task_count, 0);
    assert!(draft.reviewers.is_empty());
}

#[tokio::test]
async fn prs_open_collects_all_pages() {
    let s = MockServer::start().await;
    let one = |id: u64| {
        let mut v = fixture(include_str!("fixtures/prs_open.json"))["values"][1].clone();
        v["id"] = json!(id);
        v
    };
    Mock::given(method("GET"))
        .and(path(PRS_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"values": [one(1)], "next": format!("{}/page2", s.uri())})),
        )
        .mount(&s)
        .await;
    mount_json(&s, "/page2", json!({"values": [one(2)]})).await;
    let prs = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap()
        .prs;
    assert_eq!(prs.iter().map(|p| p.id).collect::<Vec<_>>(), vec![1, 2]);
}

#[tokio::test]
async fn prs_marks_fork_via_source_repo() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        PRS_PATH,
        fixture(include_str!("fixtures/prs_fork.json")),
    )
    .await;
    let prs = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap()
        .prs;
    assert_eq!(prs.len(), 2);
    assert_eq!(
        prs[0].source_repo,
        RepoRef::parse_bitbucket("forker/api").unwrap()
    );
    assert!(prs[0].is_fork());
    // Deleted fork: repository is null, still listed and treated as a fork.
    assert!(prs[1].is_fork());
    assert_ne!(prs[1].source_repo, repo());
}

#[tokio::test]
async fn prs_merged_stops_at_max() {
    let s = MockServer::start().await;
    let one = |id: u64| {
        let mut v = fixture(include_str!("fixtures/prs_open.json"))["values"][0].clone();
        v["id"] = json!(id);
        v["state"] = json!("MERGED");
        v
    };
    Mock::given(method("GET"))
        .and(path(PRS_PATH))
        .and(query_param("state", "MERGED"))
        .and(query_param("sort", "-updated_on"))
        .and(query_param("pagelen", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"values": [one(10), one(9), one(8)], "next": format!("{}/page2", s.uri())}),
        ))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": [one(7)]})))
        .expect(0)
        .mount(&s)
        .await;
    let prs = provider(&s)
        .prs(&repo(), PrState::Merged, 2)
        .await
        .unwrap()
        .prs;
    assert_eq!(prs.iter().map(|p| p.id).collect::<Vec<_>>(), vec![10, 9]);
    assert!(prs.iter().all(|p| p.state == PrState::Merged));
}

#[tokio::test]
async fn prs_handles_null_participant_state() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        PRS_PATH,
        fixture(include_str!("fixtures/prs_open.json")),
    )
    .await;
    let prs = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap()
        .prs;
    let dan = prs[0]
        .reviewers
        .iter()
        .find(|r| r.user.id == "{dddd}")
        .unwrap();
    assert_eq!(dan.state, ReviewState::Pending);
}

#[tokio::test]
async fn prs_handles_unicode_title() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        PRS_PATH,
        fixture(include_str!("fixtures/prs_unicode.json")),
    )
    .await;
    let prs = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap()
        .prs;
    assert_eq!(prs[0].title, "Исправить баг 日本語 🚀 café");
    assert_eq!(prs[0].author.display_name, "Zoë Müller");
    assert_eq!(prs[0].source_branch, "fix/ünï");
}

fn diffstat_page1(server: &MockServer) -> Value {
    fixture(
        &include_str!("fixtures/diffstat_page1.json")
            .replace("@NEXT@", &format!("{}/diffstat-page2", server.uri())),
    )
}

#[tokio::test]
async fn diffstat_sums_across_pages() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        "/repositories/acme/api/pullrequests/42/diffstat",
        diffstat_page1(&s),
    )
    .await;
    mount_json(
        &s,
        "/diffstat-page2",
        fixture(include_str!("fixtures/diffstat_page2.json")),
    )
    .await;
    let d = provider(&s).diffstat(&repo(), 42).await.unwrap();
    assert_eq!(d.files, 3);
    assert_eq!(d.lines_added, 310);
    assert_eq!(d.lines_removed, 52);
    assert_eq!(d.max_file_lines, 300);
}

#[tokio::test]
async fn diffstat_follows_302_to_paginated_result() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repositories/acme/api/pullrequests/42/diffstat"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", "/repositories/acme/api/diffstat/abc..def"),
        )
        .expect(1)
        .mount(&s)
        .await;
    mount_json(
        &s,
        "/repositories/acme/api/diffstat/abc..def",
        diffstat_page1(&s),
    )
    .await;
    mount_json(
        &s,
        "/diffstat-page2",
        fixture(include_str!("fixtures/diffstat_page2.json")),
    )
    .await;
    let d = provider(&s).diffstat(&repo(), 42).await.unwrap();
    assert_eq!((d.files, d.lines_added, d.lines_removed), (3, 310, 52));
}

#[tokio::test]
async fn diffstat_zero_entries_is_all_zero() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        "/repositories/acme/api/pullrequests/5/diffstat",
        fixture(include_str!("fixtures/diffstat_empty.json")),
    )
    .await;
    let d = provider(&s).diffstat(&repo(), 5).await.unwrap();
    assert_eq!(
        (d.files, d.lines_added, d.lines_removed, d.max_file_lines),
        (0, 0, 0, 0)
    );
}

/// A valid open PR entry (the draft one from the fixture) with the given id.
fn good_pr(id: u64) -> Value {
    let mut v = fixture(include_str!("fixtures/prs_open.json"))["values"][1].clone();
    v["id"] = json!(id);
    v
}

async fn open_list(values: Vec<Value>) -> PrList {
    let s = MockServer::start().await;
    mount_json(&s, PRS_PATH, json!({ "values": values })).await;
    provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap()
}

#[tokio::test]
async fn prs_tolerate_null_or_missing_commit() {
    let mut v = good_pr(1);
    v["source"]["commit"] = Value::Null;
    v["destination"].as_object_mut().unwrap().remove("commit");
    let list = open_list(vec![v]).await;
    assert!(list.skipped.is_empty(), "{:?}", list.skipped);
    assert_eq!(list.prs.len(), 1);
    assert_eq!(list.prs[0].source_commit, "");
    assert_eq!(list.prs[0].dest_commit, "");
}

#[tokio::test]
async fn prs_tolerate_null_author() {
    let mut v = good_pr(1);
    v["author"] = Value::Null;
    let mut w = good_pr(2);
    w.as_object_mut().unwrap().remove("author");
    let list = open_list(vec![v, w]).await;
    assert!(list.skipped.is_empty(), "{:?}", list.skipped);
    assert_eq!(list.prs.len(), 2);
    for pr in &list.prs {
        assert_eq!(pr.author, user("", "unknown"));
    }
}

#[tokio::test]
async fn prs_tolerate_missing_uuid_in_author_and_reviewer() {
    let mut v = fixture(include_str!("fixtures/prs_open.json"))["values"][0].clone();
    v["author"].as_object_mut().unwrap().remove("uuid");
    v["participants"][0]["user"]
        .as_object_mut()
        .unwrap()
        .remove("uuid");
    let list = open_list(vec![v]).await;
    assert!(list.skipped.is_empty(), "{:?}", list.skipped);
    let pr = &list.prs[0];
    assert_eq!(pr.author, user("", "Alice Author"));
    assert_eq!(pr.reviewers.len(), 5);
    assert_eq!(pr.reviewers[0].user, user("", "Bob Approver"));
}

#[tokio::test]
async fn prs_one_bad_entry_is_skipped_and_recorded_without_values() {
    let mut bad = good_pr(2);
    bad["title"] = json!(12345);
    bad["source"]["branch"]["name"] = json!({"secret": "SECRET-VALUE"});
    let list = open_list(vec![good_pr(1), bad, good_pr(3)]).await;
    assert_eq!(
        list.prs.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(
        list.skipped,
        vec![SkippedPr {
            id: Some(2),
            field: "title".into()
        }]
    );
    let shown = format!("{:?}", list.skipped);
    assert!(!shown.contains("12345") && !shown.contains("SECRET"));
}

#[tokio::test]
async fn prs_all_bad_entries_return_empty_list_with_every_skip() {
    let mut nested = good_pr(7);
    nested["destination"]["branch"] = Value::Null;
    let mut reviewer = good_pr(8);
    reviewer["participants"] = json!([{"role": "REVIEWER", "approved": "yes"}]);
    let list = open_list(vec![json!({"id": "not-a-number"}), nested, reviewer]).await;
    assert!(list.prs.is_empty());
    assert_eq!(
        list.skipped,
        vec![
            SkippedPr {
                id: None,
                field: "id".into()
            },
            SkippedPr {
                id: Some(7),
                field: "destination.branch.name".into()
            },
            SkippedPr {
                id: Some(8),
                field: "participants[0].approved".into()
            },
        ]
    );
}

#[tokio::test]
async fn prs_skipped_entries_do_not_count_toward_merged_max() {
    let s = MockServer::start().await;
    let merged = |id: u64| {
        let mut v = good_pr(id);
        v["state"] = json!("MERGED");
        v
    };
    mount_json(
        &s,
        PRS_PATH,
        json!({"values": [merged(10), {"id": 9}, merged(8), merged(7)]}),
    )
    .await;
    let list = provider(&s).prs(&repo(), PrState::Merged, 2).await.unwrap();
    assert_eq!(
        list.prs.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![10, 8]
    );
    assert_eq!(list.skipped.len(), 1);
}

#[tokio::test]
async fn prs_request_participants_on_open_and_merged_lists() {
    for (state, param) in [(PrState::Open, "OPEN"), (PrState::Merged, "MERGED")] {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(PRS_PATH))
            .and(query_param("state", param))
            // wiremock decodes the query: `%2B` arrives as `+` (a raw `+` would be a space).
            .and(query_param("fields", "+values.participants"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": [good_pr(1)]})))
            .expect(1)
            .mount(&s)
            .await;
        let list = provider(&s).prs(&repo(), state, 5).await.unwrap();
        assert_eq!(list.prs.len(), 1, "{param}");
    }
}

#[tokio::test]
async fn prs_next_links_keep_the_participants_field() {
    let s = MockServer::start().await;
    // The first `next` omits `fields`; the second already carries it and must not get a duplicate.
    Mock::given(method("GET"))
        .and(path(PRS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"values": [good_pr(1)], "next": format!("{}/p2?page=2", s.uri())}),
        ))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/p2"))
        .and(query_param("fields", "+values.participants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "values": [good_pr(2)],
            "next": format!("{}/p3?fields=%2Bvalues.participants&page=3", s.uri())
        })))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/p3"))
        .and(query_param("fields", "+values.participants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": [good_pr(3)]})))
        .expect(1)
        .mount(&s)
        .await;
    let list = provider(&s)
        .prs(&repo(), PrState::Open, usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        list.prs.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let reqs = s.received_requests().await.unwrap();
    let p3 = reqs.iter().find(|r| r.url.path() == "/p3").unwrap();
    assert_eq!(
        p3.url.query_pairs().filter(|(k, _)| k == "fields").count(),
        1
    );
}

#[tokio::test]
async fn prs_without_participants_key_have_no_reviewers() {
    let mut v = good_pr(1);
    v.as_object_mut().unwrap().remove("participants");
    let list = open_list(vec![v]).await;
    assert!(list.skipped.is_empty());
    assert!(list.prs[0].reviewers.is_empty());
}

#[tokio::test]
async fn branch_name_with_quotes_is_escaped_in_query() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PRS_PATH))
        .and(query_param(
            "q",
            r#"source.branch.name="we\"ird\\br" AND state="MERGED""#,
        ))
        .and(query_param("sort", "-updated_on"))
        .and(query_param("pagelen", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "values": [good_pr(4)],
            "next": format!("{}/page2", s.uri())
        })))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": []})))
        .expect(0)
        .mount(&s)
        .await;
    let list = provider(&s)
        .merged_by_source_branch(&repo(), r#"we"ird\br"#)
        .await
        .unwrap();
    assert_eq!(list.prs.len(), 1);
    assert_eq!(list.prs[0].state, PrState::Merged);
}

#[tokio::test]
async fn branch_name_with_space_unicode_and_reserved_chars_encoded_in_query() {
    let s = MockServer::start().await;
    let branch = "feat/a b&c#d+e%f\u{e9}\u{4e2d}";
    Mock::given(method("GET"))
        .and(path(PRS_PATH))
        .and(query_param(
            "q",
            format!("source.branch.name=\"{branch}\" AND state=\"MERGED\""),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": [good_pr(5)]})))
        .expect(1)
        .mount(&s)
        .await;
    let list = provider(&s)
        .merged_by_source_branch(&repo(), branch)
        .await
        .unwrap();
    assert_eq!(list.prs.len(), 1);
    let reqs = s.received_requests().await.unwrap();
    let raw = reqs[0].url.as_str();
    let query = raw.split_once('?').unwrap().1;
    assert!(!query.contains(' ') && !query.contains('#'), "{query}");
    // exactly the four parameters: q, sort, pagelen, fields (no stray `&` from the branch)
    assert_eq!(query.split('&').count(), 4, "{query}");
}

// ---- comments and review actions ----

use prcore::comments::{Anchor, NewComment, ReviewAction};
use prcore::draft::Side;
use providers::provider::ProviderError;
use wiremock::matchers::body_json;

const PR1: &str = "/repositories/acme/api/pullrequests/1";

fn ts(s: &str) -> chrono::DateTime<Utc> {
    s.parse().unwrap()
}

#[tokio::test]
async fn comments_paged_and_threadable() {
    let s = MockServer::start().await;
    let p1 = include_str!("fixtures/comments_page1.json")
        .replace("__NEXT__", &format!("{}{PR1}/comments?page=2", s.uri()));
    Mock::given(method("GET"))
        .and(path(format!("{PR1}/comments")))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(fixture(include_str!("fixtures/comments_page2.json"))),
        )
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(&p1)))
        .mount(&s)
        .await;
    let list = provider(&s).comments(&repo(), 1).await.unwrap();
    // Comment 3 has no content: it is kept with an empty body, not skipped.
    assert_eq!(list.comments.len(), 6);
    assert!(list.skipped.is_empty());
    assert_eq!(
        list.comments
            .iter()
            .find(|c| c.id == CommentId::from(3))
            .unwrap()
            .body,
        ""
    );
    let c1 = &list.comments[0];
    assert_eq!(c1.id, CommentId::from(1));
    assert_eq!(c1.parent_id, None);
    assert!(c1.resolved);
    assert_eq!(c1.body, "root inline");
    assert_eq!(
        c1.author,
        user("{11111111-1111-1111-1111-111111111111}", "Ada Lovelace")
    );
    assert_eq!(c1.created_on, ts("2026-01-01T10:00:00Z"));
    assert_eq!(
        c1.anchor,
        Some(Anchor {
            path: "src/a.rs".into(),
            from: None,
            to: Some(12),
            start: None,
            outdated: false,
            old_path: None,
            commit: None,
        })
    );
    let c2 = &list.comments[1];
    assert_eq!(c2.parent_id, Some(CommentId::from(1)));
    assert!(!c2.resolved);
    let old = list
        .comments
        .iter()
        .find(|c| c.id == CommentId::from(5))
        .unwrap();
    assert!(old.deleted);
    assert_eq!(old.anchor.as_ref().unwrap().from, Some(7));
    let file = list
        .comments
        .iter()
        .find(|c| c.id == CommentId::from(6))
        .unwrap();
    assert_eq!(
        file.anchor,
        Some(Anchor {
            path: "src/c.rs".into(),
            from: None,
            to: None,
            start: None,
            outdated: false,
            old_path: None,
            commit: None,
        })
    );
    assert!(list
        .comments
        .iter()
        .find(|c| c.id == CommentId::from(4))
        .unwrap()
        .anchor
        .is_none());
    let threads = prcore::comments::build_threads(list.comments);
    let t1 = threads
        .iter()
        .find(|t| t.root.id == CommentId::from(1))
        .unwrap();
    assert_eq!(t1.replies.len(), 1);
    assert_eq!(t1.replies[0].id, CommentId::from(2));
}

#[tokio::test]
async fn comment_with_missing_content_is_kept_with_empty_body() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        &format!("{PR1}/comments"),
        json!({"values":[
            {"id":9,"user":{"display_name":"X","uuid":"{x}"},"created_on":"2026-01-01T10:00:00Z","updated_on":"2026-01-01T10:00:00Z"},
            {"id":10,"user":{"display_name":"X","uuid":"{x}"},"content":{"raw":"ok"},"created_on":"2026-01-01T10:00:00Z","updated_on":"2026-01-01T10:00:00Z"}
        ]}),
    )
    .await;
    let list = provider(&s).comments(&repo(), 1).await.unwrap();
    assert_eq!(list.comments.len(), 2);
    assert_eq!(list.comments[0].id, CommentId::from(9));
    assert_eq!(list.comments[0].body, "");
    assert_eq!(list.comments[1].body, "ok");
    assert!(list.skipped.is_empty());
}

// ---- bot, app and incomplete comments ----

async fn comments_of(values: Vec<Value>) -> providers::provider::CommentList {
    let s = MockServer::start().await;
    mount_json(&s, &format!("{PR1}/comments"), json!({ "values": values })).await;
    provider(&s).comments(&repo(), 1).await.unwrap()
}

fn entry(id: u64, extra: Value) -> Value {
    let mut v = json!({
        "id": id,
        "user": {"type":"user","display_name":"X","uuid":"{x}"},
        "content": {"raw":"body"},
        "created_on": "2026-01-01T10:00:00Z",
        "updated_on": "2026-01-01T10:00:00Z"
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

#[tokio::test]
async fn app_user_without_uuid_or_display_name_is_decoded() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        &format!("{PR1}/comments"),
        fixture(include_str!("fixtures/comments_bots.json")),
    )
    .await;
    let list = provider(&s).comments(&repo(), 1).await.unwrap();
    assert!(list.skipped.is_empty(), "{:?}", list.skipped);
    assert_eq!(list.comments.len(), 7);
    let by = |id: u64| {
        list.comments
            .iter()
            .find(|c| c.id == CommentId::from(id))
            .unwrap()
    };
    // app_user with nothing but its type
    let rovo = by(101);
    assert_eq!(rovo.author, user("", "Unknown author"));
    assert!(rovo.author_is_bot);
    assert_eq!(rovo.body, "Rovo Dev found a possible null dereference.");
    assert_eq!(rovo.anchor.as_ref().unwrap().to, Some(3));
    // app_user with only an account_id
    assert_eq!(by(102).author, user("5d1c:rovo-app", "Unknown author"));
    assert!(by(102).author_is_bot);
    // only a display name: recognised by name
    assert_eq!(by(103).author, user("", "Rovo Dev"));
    assert!(by(103).author_is_bot);
    // no account_id or display_name: nickname is the name
    assert_eq!(
        by(104).author,
        user(
            "{33333333-3333-3333-3333-333333333333}",
            "sonarcloud-reviewer"
        )
    );
    assert!(by(104).author_is_bot);
    // a person is not a bot
    assert!(!by(105).author_is_bot);
    assert_eq!(by(105).author.display_name, "Ada Lovelace");
    // null user
    assert_eq!(by(106).author, user("", "Unknown author"));
    assert!(!by(106).author_is_bot);
    // team accounts post as automation
    assert!(by(107).author_is_bot);
}

#[tokio::test]
async fn html_only_body_falls_back_to_plain_text() {
    let html = "<p>Line &lt;one&gt; &quot;q&quot; &#39;s&#39;&nbsp;x &amp; y</p>\n\
                <ul>\n<li>a</li>\n<li>b</li>\n</ul>\n\
                <p>line<br>two<br/>three<br>\
                <script>alert('x')</script><style>p { color: red }</style>end</p>";
    let list = comments_of(vec![
        entry(
            1,
            json!({"content":{"raw":"","markup":"markdown","html":html}}),
        ),
        entry(2, json!({"content":{"html":"<b>only</b> html"}})),
        entry(
            3,
            json!({"content":{"raw":"raw wins","html":"<p>not this</p>"}}),
        ),
    ])
    .await;
    assert_eq!(
        list.comments[0].body,
        "Line <one> \"q\" 's' x & y\na\nb\nline\ntwo\nthree\nend"
    );
    assert_eq!(list.comments[1].body, "only html");
    assert_eq!(list.comments[2].body, "raw wins");
}

#[tokio::test]
async fn ranged_inline_uses_end_line_and_keeps_start() {
    let list = comments_of(vec![
        entry(
            1,
            json!({"inline":{"path":"src/a.rs","start_to":10,"to":14,"start_from":null,"from":null}}),
        ),
        entry(
            2,
            json!({"inline":{"path":"src/a.rs","start_from":3,"from":5,"to":null}}),
        ),
        // start only, no end: the start is the anchor line
        entry(3, json!({"inline":{"path":"src/a.rs","start_to":8}})),
        // single line: no start
        entry(
            4,
            json!({"inline":{"path":"src/a.rs","start_to":9,"to":9}}),
        ),
    ])
    .await;
    let a = |i: usize| list.comments[i].anchor.clone().unwrap();
    assert_eq!(
        a(0),
        Anchor {
            path: "src/a.rs".into(),
            from: None,
            to: Some(14),
            start: Some((Side::New, 10)),
            outdated: false,
            old_path: None,
            commit: None,
        }
    );
    assert_eq!(
        (a(1).from, a(1).to, a(1).start_line()),
        (Some(5), None, Some(3))
    );
    assert_eq!(
        (a(2).from, a(2).to, a(2).start_line()),
        (None, Some(8), None)
    );
    assert_eq!((a(3).to, a(3).start_line()), (Some(9), None));
}

/// `inline.outdated` as in Bitbucket's example responses (with `context_lines`), absent,
/// or not a boolean; the line numbers are kept as sent either way.
#[tokio::test]
async fn inline_outdated_flag_is_decoded_tolerantly() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        &format!("{PR1}/comments"),
        fixture(include_str!("fixtures/comments_outdated.json")),
    )
    .await;
    let list = provider(&s).comments(&repo(), 1).await.unwrap();
    assert!(list.skipped.is_empty(), "{:?}", list.skipped);
    let a = |id: u64| {
        list.comments
            .iter()
            .find(|c| c.id == CommentId::from(id))
            .unwrap()
            .anchor
            .clone()
            .unwrap()
    };
    assert_eq!(
        a(201),
        Anchor {
            path: "docker-compose.yaml".into(),
            from: None,
            to: Some(14),
            start: None,
            outdated: true,
            old_path: None,
            commit: None,
        }
    );
    assert_eq!(
        (a(202).from, a(202).to, a(202).outdated),
        (Some(211), None, true)
    );
    assert!(!a(203).outdated, "explicit false");
    assert!(!a(204).outdated, "no flag");
    assert!(!a(205).outdated, "a string is not a boolean");
    assert_eq!(a(205).to, Some(5));
    let file = a(206);
    assert_eq!((file.from, file.to, file.outdated), (None, None, true));
}

#[tokio::test]
async fn zero_or_negative_inline_line_becomes_file_level() {
    let list = comments_of(vec![
        entry(1, json!({"inline":{"path":"src/a.rs","to":0}})),
        entry(2, json!({"inline":{"path":"src/a.rs","from":-3,"to":null}})),
        entry(3, json!({"inline":{"path":"src/a.rs","to":"12"}})),
        entry(4, json!({"inline":{"path":"src/a.rs","to":1.5,"from":4}})),
        entry(
            5,
            json!({"inline":{"path":"src/a.rs","to":5_000_000_000u64}}),
        ),
        // inline without a usable path: a PR-level comment
        entry(6, json!({"inline":{"to":3}})),
    ])
    .await;
    assert!(list.skipped.is_empty());
    assert_eq!(list.comments.len(), 6);
    for c in &list.comments[..5] {
        assert_eq!(
            c.anchor,
            Some(Anchor {
                path: "src/a.rs".into(),
                from: None,
                to: None,
                start: None,
                outdated: false,
                old_path: None,
                commit: None,
            }),
            "comment {}",
            c.id
        );
    }
    assert_eq!(list.comments[5].anchor, None);
}

#[tokio::test]
async fn comment_without_id_is_skipped_and_counted() {
    let list = comments_of(vec![
        json!({"user":{}, "content":{"raw":"no id"}}),
        json!({"id":"12", "content":{"raw":"string id"}}),
        json!({"id":-1, "content":{"raw":"negative id"}}),
        // no timestamps, no user, no content: still a comment
        json!({"id":13}),
    ])
    .await;
    assert_eq!(list.comments.len(), 1);
    let c = &list.comments[0];
    assert_eq!(c.id, CommentId::from(13));
    assert_eq!(c.author.display_name, "Unknown author");
    assert_eq!(c.body, "");
    assert_eq!(list.skipped.len(), 3);
    assert!(list
        .skipped
        .iter()
        .all(|s| s.id.is_none() && s.field == "id"));
}

#[tokio::test]
async fn missing_timestamp_falls_back_to_the_other() {
    let list = comments_of(vec![json!({"id":1,"updated_on":"2026-01-02T10:00:00Z"})]).await;
    let c = &list.comments[0];
    assert_eq!(c.created_on, ts("2026-01-02T10:00:00Z"));
    assert_eq!(c.updated_on, ts("2026-01-02T10:00:00Z"));
}

#[tokio::test]
async fn huge_body_is_kept_intact() {
    let body: String = (0..100 * 1024)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let list = comments_of(vec![entry(1, json!({"content":{"raw":body.clone()}}))]).await;
    assert_eq!(list.comments[0].body.len(), 100 * 1024);
    assert_eq!(list.comments[0].body, body);
}

#[tokio::test]
async fn reply_to_deleted_parent_becomes_root() {
    let list = comments_of(vec![
        entry(1, json!({})),
        // parent 999 was deleted and is not in the list
        entry(2, json!({"parent":{"id":999,"deleted":true}})),
        entry(3, json!({"parent":{"id":1}})),
    ])
    .await;
    let threads = prcore::comments::build_threads(list.comments);
    let roots: Vec<u64> = threads
        .iter()
        .map(|t| t.root.id.as_u64().unwrap())
        .collect();
    assert_eq!(roots, vec![1, 2]);
    assert_eq!(threads[0].replies[0].id, CommentId::from(3));
}

#[tokio::test]
async fn all_pages_are_fetched() {
    let s = MockServer::start().await;
    let page = |ids: &[u64], next: Option<String>| {
        let mut v = json!({"values": ids.iter().map(|&i| entry(i, json!({}))).collect::<Vec<_>>()});
        if let Some(n) = next {
            v["next"] = json!(n);
        }
        v
    };
    let base = format!("{}{PR1}/comments", s.uri());
    for (n, ids, next) in [
        (
            "2",
            vec![3, 4],
            Some(format!("{base}?sort=created_on&pagelen=100&page=3")),
        ),
        ("3", vec![5], None),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("{PR1}/comments")))
            .and(query_param("page", n))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(&ids, next)))
            .expect(1)
            .mount(&s)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(format!("{PR1}/comments")))
        .and(query_param("sort", "created_on"))
        .and(query_param("pagelen", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            &[1, 2],
            Some(format!("{base}?sort=created_on&pagelen=100&page=2")),
        )))
        .expect(1)
        .mount(&s)
        .await;
    let list = provider(&s).comments(&repo(), 1).await.unwrap();
    let ids: Vec<u64> = list
        .comments
        .iter()
        .map(|c| c.id.as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);

    // A `next` link to another origin is refused: no credentials leave the origin.
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(&[9], None)))
        .expect(0)
        .mount(&other)
        .await;
    let s2 = MockServer::start().await;
    mount_json(
        &s2,
        &format!("{PR1}/comments"),
        page(&[1], Some(format!("{}{PR1}/comments?page=2", other.uri()))),
    )
    .await;
    let err = provider(&s2).comments(&repo(), 1).await.unwrap_err();
    assert!(matches!(err, ProviderError::Network(_)), "{err:?}");
}

#[tokio::test]
async fn tasks_are_decoded_read_only() {
    let s = MockServer::start().await;
    mount_json(&s, &format!("{PR1}/comments"), json!({"values":[]})).await;
    Mock::given(method("GET"))
        .and(path(format!("{PR1}/tasks")))
        .and(query_param("pagelen", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values":[
            {"id":1,"state":"UNRESOLVED","content":{"raw":"fix the null check"},
             "creator":{"type":"app_user","display_name":"Rovo Dev"},
             "comment":{"id":101},"created_on":"2026-01-01T10:00:00Z","updated_on":"2026-01-01T10:00:00Z"},
            {"id":2,"state":"RESOLVED","content":{"raw":"","html":"<p>add &amp; test</p>"},
             "creator":{"display_name":"Ada","uuid":"{a}"}},
            {"id":3,"state":"UNRESOLVED","pending":true,"content":{"raw":"draft"}},
            {"state":"UNRESOLVED","content":{"raw":"no id"}}
        ]})))
        .mount(&s)
        .await;
    let p = provider(&s);
    let tasks = p.tasks(&repo(), 1).await.unwrap();
    use prcore::comments::{PrTask, TaskState};
    assert_eq!(
        tasks,
        vec![
            PrTask {
                id: 1,
                state: TaskState::Open,
                content: "fix the null check".into(),
                creator: user("", "Rovo Dev"),
                comment_id: Some(CommentId::from(101)),
            },
            PrTask {
                id: 2,
                state: TaskState::Resolved,
                content: "add & test".into(),
                creator: user("{a}", "Ada"),
                comment_id: None,
            },
        ]
    );
    // The comment list carries them too.
    let list = p.comments(&repo(), 1).await.unwrap();
    assert_eq!(list.tasks.as_deref(), Some(&tasks[..]));
}

#[tokio::test]
async fn tasks_failure_does_not_fail_comments() {
    // No tasks endpoint mounted (404): comments still load, tasks are unknown.
    let list = comments_of(vec![entry(1, json!({}))]).await;
    assert_eq!(list.comments.len(), 1);
    assert_eq!(list.tasks, None);
}

async fn expect_post(s: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .and(body_json(body))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(fixture(include_str!("fixtures/comment_posted.json"))),
        )
        .expect(1)
        .mount(s)
        .await;
}

#[tokio::test]
async fn post_inline_new_side_sends_to_only() {
    let s = MockServer::start().await;
    expect_post(
        &s,
        json!({"content":{"raw":"hello"},"inline":{"path":"src/a.rs","to":12}}),
    )
    .await;
    let c = provider(&s)
        .post_comment(
            &repo(),
            1,
            &at(),
            &NewComment::Inline {
                path: "src/a.rs".into(),
                side: Side::New,
                line: 12,
                start: None,
                text: "hello".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(c.id, CommentId::from(77));
    assert_eq!(c.body, "hello");
}

#[tokio::test]
async fn post_inline_old_side_sends_from_only() {
    let s = MockServer::start().await;
    expect_post(
        &s,
        json!({"content":{"raw":"hello"},"inline":{"path":"src/a.rs","from":3}}),
    )
    .await;
    provider(&s)
        .post_comment(
            &repo(),
            1,
            &at(),
            &NewComment::Inline {
                path: "src/a.rs".into(),
                side: Side::Old,
                line: 3,
                start: None,
                text: "hello".into(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn post_reply_sends_parent() {
    let s = MockServer::start().await;
    expect_post(&s, json!({"content":{"raw":"hello"},"parent":{"id":1}})).await;
    provider(&s)
        .post_comment(
            &repo(),
            1,
            &at(),
            &NewComment::Reply {
                thread: ThreadId::from(1),
                text: "hello".into(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn post_file_level_has_path_only() {
    let s = MockServer::start().await;
    expect_post(
        &s,
        json!({"content":{"raw":"hello"},"inline":{"path":"src/a.rs"}}),
    )
    .await;
    provider(&s)
        .post_comment(
            &repo(),
            1,
            &at(),
            &NewComment::File {
                path: "src/a.rs".into(),
                text: "hello".into(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn post_pr_level_has_content_only() {
    let s = MockServer::start().await;
    expect_post(&s, json!({"content":{"raw":"hello"}})).await;
    provider(&s)
        .post_comment(
            &repo(),
            1,
            &at(),
            &NewComment::Pr {
                text: "hello".into(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn post_non_2xx_returns_error_without_token() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(ResponseTemplate::new(400).set_body_string("tok bad line"))
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Http(400));
    assert!(!format!("{err} {err:?}").contains("tok"));
}

#[tokio::test]
async fn resolve_and_unresolve_use_post_and_delete() {
    let s = MockServer::start().await;
    let p = format!("{PR1}/comments/5/resolve");
    Mock::given(method("POST"))
        .and(path(p.clone()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"type":"comment_resolution"})),
        )
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&s)
        .await;
    let pr = provider(&s);
    pr.set_resolved(&repo(), 1, &ThreadId::from(5), true)
        .await
        .unwrap();
    pr.set_resolved(&repo(), 1, &ThreadId::from(5), false)
        .await
        .unwrap();
}

#[tokio::test]
async fn approve_request_changes_clear_use_correct_verbs() {
    let s = MockServer::start().await;
    for (verb, tail, status) in [
        ("POST", "approve", 200),
        ("DELETE", "approve", 204),
        ("POST", "request-changes", 200),
        ("DELETE", "request-changes", 204),
    ] {
        Mock::given(method(verb))
            .and(path(format!("{PR1}/{tail}")))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&s)
            .await;
    }
    let pr = provider(&s);
    for a in [
        ReviewAction::Approve,
        ReviewAction::ClearApproval,
        ReviewAction::RequestChanges,
        ReviewAction::ClearRequestChanges,
    ] {
        pr.set_review(&repo(), 1, a, None).await.unwrap();
    }
}

#[tokio::test]
async fn review_action_errors_map_status() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/approve")))
        .respond_with(ResponseTemplate::new(409))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("{PR1}/approve")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    let pr = provider(&s);
    assert_eq!(
        pr.set_review(&repo(), 1, ReviewAction::Approve, None).await,
        Err(ProviderError::Http(409))
    );
    assert_eq!(
        pr.set_review(&repo(), 1, ReviewAction::ClearApproval, None)
            .await,
        Err(ProviderError::NotFound)
    );
}

#[tokio::test]
async fn pr_fetch_returns_current_source_commit() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        PR1,
        json!({"id":1,"title":"T","state":"OPEN",
          "source":{"branch":{"name":"feat"},"commit":{"hash":"abc123"},"repository":{"full_name":"acme/api"}},
          "destination":{"branch":{"name":"main"},"commit":{"hash":"def456"}},
          "created_on":"2026-01-01T10:00:00Z","updated_on":"2026-01-02T10:00:00Z"}),
    )
    .await;
    let pr = provider(&s).pr(&repo(), 1).await.unwrap();
    assert_eq!(pr.id, 1);
    assert_eq!(pr.source_commit, "abc123");
    assert_eq!(pr.dest_commit, "def456");
    assert_eq!(pr.state, PrState::Open);
}

// ---- review fixes ----

#[tokio::test]
async fn post_inline_line_zero_rejected_before_http() {
    let s = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&s)
        .await;
    for side in [Side::New, Side::Old] {
        let err = provider(&s)
            .post_comment(
                &repo(),
                1,
                &at(),
                &NewComment::Inline {
                    path: "a".into(),
                    side,
                    line: 0,
                    start: None,
                    text: "x".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");
    }
}

#[tokio::test]
async fn pending_comments_are_skipped_and_counted() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        &format!("{PR1}/comments"),
        json!({"values":[
            {"id":20,"pending":true,"user":{"display_name":"X","uuid":"{x}"},"content":{"raw":"draft"},"created_on":"2026-01-01T10:00:00Z","updated_on":"2026-01-01T10:00:00Z"},
            {"id":21,"pending":false,"user":{"display_name":"X","uuid":"{x}"},"content":{"raw":"real"},"created_on":"2026-01-01T10:00:00Z","updated_on":"2026-01-01T10:00:00Z"}
        ]}),
    )
    .await;
    let list = provider(&s).comments(&repo(), 1).await.unwrap();
    assert_eq!(list.comments.len(), 1);
    assert_eq!(list.comments[0].id, CommentId::from(21));
    assert_eq!(list.skipped.len(), 1);
    assert_eq!(list.skipped[0].id, Some(20));
    assert_eq!(list.skipped[0].field, "pending");
}

#[tokio::test]
async fn post_2xx_with_unreadable_body_is_posted_unreadable() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
        .expect(1)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::PostedUnreadable);
}

#[tokio::test]
async fn post_redirect_is_an_error_and_not_followed() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/elsewhere"))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/elsewhere"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(0)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Http(302));
}

#[tokio::test]
async fn post_5xx_is_not_retried() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    // sent, and the server failed while handling it: it may have been created
    assert_eq!(err, ProviderError::Unconfirmed);
}

#[tokio::test]
async fn post_429_then_201_succeeds_with_two_requests() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(fixture(include_str!("fixtures/comment_posted.json"))),
        )
        .expect(1)
        .mount(&s)
        .await;
    let c = provider(&s)
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap();
    assert_eq!(c.id, CommentId::from(77));
    assert_eq!(s.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn post_body_round_trips_unicode_newlines_and_quotes() {
    let s = MockServer::start().await;
    let text = "héllo \"quoted\" \\ back\nline2\t😀 日本";
    expect_post(&s, json!({"content":{"raw":text}})).await;
    provider(&s)
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: text.into() })
        .await
        .unwrap();
}

#[tokio::test]
async fn real_token_never_appears_in_post_errors() {
    let token = "s3cr3t-T0KEN-v4lue";
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(token))
        .mount(&s)
        .await;
    let p = BitbucketProvider::new(BbClient::new(&s.uri(), "me@example.com", token));
    let err = p
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert!(!format!("{err} {err:?}").contains(token));
}

// ---- outcome of a sent comment ----

/// A one-shot HTTP server: reads one request, writes `answer` raw, then closes.
async fn raw_server(answer: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = l.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // the whole request (headers and the small JSON body) before answering
        loop {
            let n = sock.read(&mut chunk).await.unwrap();
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
            if n == 0 {
                break;
            }
        }
        sock.write_all(answer.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn decode_failure_after_2xx_is_posted_unreadable() {
    // a body that is not JSON
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_string("<html>ok</html>"))
        .mount(&s)
        .await;
    let err = provider(&s)
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::PostedUnreadable);
    // a body cut off mid-way (it could not be read at all)
    let base = raw_server(
        "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 400\r\n\r\n{\"id\":",
    )
    .await;
    let p = BitbucketProvider::new(BbClient::new(&base, "me@example.com", "t"));
    let err = p
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::PostedUnreadable);
}

#[tokio::test]
async fn timeout_after_send_is_unconfirmed() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments")))
        .respond_with(ResponseTemplate::new(201).set_delay(std::time::Duration::from_secs(3)))
        .mount(&s)
        .await;
    let p = BitbucketProvider::new(BbClient::with_timeout(
        &s.uri(),
        "me@example.com",
        "t",
        std::time::Duration::from_millis(300),
    ));
    let err = p
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Unconfirmed);
    // every 5xx after sending is unknown as well; a 4xx is a definite refusal
    for (status, want) in [
        (500, ProviderError::Unconfirmed),
        (502, ProviderError::Unconfirmed),
        (504, ProviderError::Unconfirmed),
        (400, ProviderError::Http(400)),
    ] {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&s)
            .await;
        let err = provider(&s)
            .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
            .await
            .unwrap_err();
        assert_eq!(err, want, "{status}");
    }
    // a connection reset after the request went out
    let base = raw_server("").await;
    let p = BitbucketProvider::new(BbClient::new(&base, "me@example.com", "t"));
    let err = p
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Unconfirmed);
}

#[tokio::test]
async fn connect_failure_is_definite() {
    // nothing listens on port 1: the request never left
    let p = BitbucketProvider::new(BbClient::new("http://127.0.0.1:1", "me@example.com", "t"));
    let err = p
        .post_comment(&repo(), 1, &at(), &NewComment::Pr { text: "x".into() })
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Network(_)), "{err:?}");
}

#[tokio::test]
async fn review_actions_succeed_on_2xx_whatever_the_body() {
    let base = raw_server(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 400\r\n\r\n{\"appr",
    )
    .await;
    let p = BitbucketProvider::new(BbClient::new(&base, "me@example.com", "t"));
    p.set_review(&repo(), 1, ReviewAction::Approve, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn write_refusals_carry_bitbucket_message() {
    let s = MockServer::start().await;
    // approving my own PR
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/approve")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": {"message": "You can't approve your own pull request."}
        })))
        .mount(&s)
        .await;
    // approving twice; a long multi-line message with control characters
    let long = format!("Already\napproved.\u{7}\t{}", "x".repeat(400));
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/request-changes")))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(json!({"type": "error", "error": {"message": long}})),
        )
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("{PR1}/approve")))
        .respond_with(ResponseTemplate::new(403).set_body_string("not json"))
        .mount(&s)
        .await;
    let pr = provider(&s);
    assert_eq!(
        pr.set_review(&repo(), 1, ReviewAction::Approve, None).await,
        Err(ProviderError::Rejected {
            status: 400,
            message: "You can't approve your own pull request.".into()
        })
    );
    match pr
        .set_review(&repo(), 1, ReviewAction::RequestChanges, None)
        .await
    {
        Err(ProviderError::Rejected { status, message }) => {
            assert_eq!(status, 409);
            assert!(message.starts_with("Already approved. "), "{message:?}");
            assert!(message.chars().count() <= 160, "{}", message.len());
            assert!(!message.chars().any(char::is_control), "{message:?}");
        }
        other => panic!("{other:?}"),
    }
    // no message to show: the plain mapping
    assert_eq!(
        pr.set_review(&repo(), 1, ReviewAction::ClearApproval, None)
            .await,
        Err(ProviderError::Forbidden)
    );
}

// ---- Provider v2: capabilities, revision, thread ids ----

#[test]
fn bitbucket_capabilities_match_spec_table() {
    use prcore::comments::AnchorModel;
    use providers::provider::Capabilities;
    let s = offline_provider();
    assert_eq!(s.forge(), prcore::model::Forge::BitbucketCloud);
    let c = s.capabilities();
    assert_eq!(
        c,
        Capabilities {
            request_changes: true,
            clear_approval: true,
            clear_request_changes: true,
            resolve_threads: true,
            reply_to_general_comments: true,
            file_comments: true,
            multi_line_comments: false,
            suggestions: None,
            tasks: true,
            inline_anchor: AnchorModel::CurrentDiff,
            batch_review: false,
            read_only: false,
        }
    );
    for a in [
        ReviewAction::Approve,
        ReviewAction::RequestChanges,
        ReviewAction::ClearApproval,
        ReviewAction::ClearRequestChanges,
    ] {
        assert!(c.allows(a), "{a:?}");
    }
}

/// A provider whose client points nowhere: for checks that make no request.
fn offline_provider() -> BitbucketProvider {
    BitbucketProvider::new(BbClient::new(
        "https://127.0.0.1:9",
        "me@example.com",
        "tok",
    ))
}

#[tokio::test]
async fn revision_is_the_current_head_and_destination() {
    let s = MockServer::start().await;
    mount_json(
        &s,
        PR1,
        json!({"id":1,"title":"T","state":"OPEN",
          "source":{"branch":{"name":"feat"},"commit":{"hash":"abc123"},"repository":{"full_name":"acme/api"}},
          "destination":{"branch":{"name":"main"},"commit":{"hash":"def456"}},
          "created_on":"2026-01-01T10:00:00Z","updated_on":"2026-01-02T10:00:00Z"}),
    )
    .await;
    let r = provider(&s).revision(&repo(), 1).await.unwrap();
    assert_eq!(
        (r.head.as_str(), r.base, r.dest_branch.as_str()),
        ("abc123", None, "main")
    );
}

#[tokio::test]
async fn set_resolved_uses_thread_root_id_and_refuses_other_ids() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/comments/42/resolve")))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let pr = provider(&s);
    pr.set_resolved(&repo(), 1, &ThreadId::from(42), true)
        .await
        .unwrap();
    // a thread id this provider never made is refused before any request
    let err = pr
        .set_resolved(&repo(), 1, &ThreadId("PRRT_kwDO".into()), true)
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");
}

#[tokio::test]
async fn set_review_ignores_body_on_bitbucket() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{PR1}/request-changes")))
        .and(wiremock::matchers::body_string(""))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    provider(&s)
        .set_review(&repo(), 1, ReviewAction::RequestChanges, Some("please fix"))
        .await
        .unwrap();
}

#[tokio::test]
async fn multi_line_or_foreign_reply_is_refused_before_any_request() {
    let s = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&s)
        .await;
    let pr = provider(&s);
    let multi = NewComment::Inline {
        path: "a.rs".into(),
        side: Side::New,
        line: 9,
        start: Some((Side::New, 7)),
        text: "x".into(),
    };
    let reply = NewComment::Reply {
        thread: ThreadId("note:1".into()),
        text: "x".into(),
    };
    for c in [multi, reply] {
        let err = pr.post_comment(&repo(), 1, &at(), &c).await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");
    }
}

/// Bitbucket's transport with nothing allowed but approving.
struct ApproveOnly(BitbucketProvider);

#[async_trait::async_trait]
impl Provider for ApproveOnly {
    fn forge(&self) -> prcore::model::Forge {
        self.0.forge()
    }
    fn capabilities(&self) -> providers::provider::Capabilities {
        providers::provider::Capabilities {
            request_changes: false,
            clear_approval: false,
            clear_request_changes: false,
            ..self.0.capabilities()
        }
    }
    async fn me(&self) -> Result<UserRef, ProviderError> {
        self.0.me().await
    }
    async fn discover(&self) -> Result<providers::provider::Discovered, ProviderError> {
        self.0.discover().await
    }
    async fn pr(&self, r: &RepoRef, id: u64) -> Result<prcore::model::PullRequest, ProviderError> {
        self.0.pr(r, id).await
    }
    async fn revision(
        &self,
        r: &RepoRef,
        id: u64,
    ) -> Result<providers::provider::ServerRevision, ProviderError> {
        self.0.revision(r, id).await
    }
    async fn comments(
        &self,
        r: &RepoRef,
        id: u64,
    ) -> Result<providers::provider::CommentList, ProviderError> {
        self.0.comments(r, id).await
    }
    async fn tasks(
        &self,
        r: &RepoRef,
        id: u64,
    ) -> Result<Vec<prcore::comments::PrTask>, ProviderError> {
        self.0.tasks(r, id).await
    }
    async fn post_comment(
        &self,
        r: &RepoRef,
        id: u64,
        at: &PostRevision,
        c: &NewComment,
    ) -> Result<prcore::comments::Comment, ProviderError> {
        self.0.post_comment(r, id, at, c).await
    }
    async fn set_resolved(
        &self,
        r: &RepoRef,
        id: u64,
        t: &ThreadId,
        resolved: bool,
    ) -> Result<(), ProviderError> {
        self.0.set_resolved(r, id, t, resolved).await
    }
    async fn prs(&self, r: &RepoRef, st: PrState, max: usize) -> Result<PrList, ProviderError> {
        self.0.prs(r, st, max).await
    }
    async fn merged_by_source_branch(&self, r: &RepoRef, b: &str) -> Result<PrList, ProviderError> {
        self.0.merged_by_source_branch(r, b).await
    }
    async fn diffstat(
        &self,
        r: &RepoRef,
        id: u64,
    ) -> Result<prcore::model::DiffStat, ProviderError> {
        self.0.diffstat(r, id).await
    }
    async fn set_review(
        &self,
        r: &RepoRef,
        id: u64,
        a: ReviewAction,
        body: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.0.set_review(r, id, a, body).await
    }
}

#[tokio::test]
async fn unsupported_action_is_refused_before_any_request() {
    use providers::provider::set_review_checked;
    let s = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&s)
        .await;
    let p = ApproveOnly(provider(&s));
    for a in [
        ReviewAction::RequestChanges,
        ReviewAction::ClearApproval,
        ReviewAction::ClearRequestChanges,
    ] {
        let err = set_review_checked(&p, &repo(), 1, a, None)
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{a:?}: {err:?}");
    }
}

/// The revision a review was fetched at; Bitbucket ignores it when posting.
fn at() -> PostRevision {
    PostRevision {
        head: "abc123".into(),
        merge_base: "def456".into(),
    }
}

#[tokio::test]
async fn discover_lists_every_workspace_and_reports_failed_ones_without_err() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": [
            {"workspace": {"slug": "acme"}},
            {"workspace": {"slug": "locked"}}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/repositories/acme"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture(include_str!("fixtures/repos.json"))),
        )
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/repositories/locked"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&s)
        .await;
    let d = provider(&s).discover().await.unwrap();
    assert_eq!(d.namespaces, vec!["acme".to_string(), "locked".to_string()]);
    assert_eq!(
        d.failed,
        vec![("locked".to_string(), ProviderError::Forbidden)]
    );
    assert_eq!(d.listed_namespaces(), vec!["acme".to_string()]);
    let names: Vec<String> = d.repos.iter().map(|r| r.repo.full_name()).collect();
    assert_eq!(names, vec!["acme/api", "acme/web"]);
}

#[tokio::test]
async fn discover_auth_failure_in_a_workspace_is_err() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": [
            {"workspace": {"slug": "acme"}}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/repositories/acme"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&s)
        .await;
    assert_eq!(
        provider(&s).discover().await.unwrap_err(),
        ProviderError::Auth
    );
}
