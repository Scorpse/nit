//! GitLabProvider discussions read (spec 7.4) against a fake GitLab serving the docs-derived
//! fixtures of `tests/fixtures/gitlab/` (see `tests/fixtures/gitlab/SOURCES.md`).

use prcore::comments::{Comment, CommentId, ThreadId};
use prcore::draft::Side;
use prcore::model::RepoRef;
use providers::gitlab::GitLabProvider;
use providers::provider::{CommentList, Provider, SkippedComment};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HOST: &str = "gitlab.example.com";
const MR: &str = "/projects/my-group%2Fsub%2Fmy-project/merge_requests/10";
const HEAD: &str = "33e2ee8579fda5bc36accc9c6fbd0b4fefda9e30";

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

async fn read() -> CommentList {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(MR))
        .respond_with(ok("mr_detail"))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR}/versions")))
        .respond_with(ok("mr_versions"))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR}/discussions")))
        .and(query_param("per_page", "100"))
        .respond_with(ok("discussions"))
        .expect(1)
        .mount(&s)
        .await;
    let p = GitLabProvider::new(&s.uri(), HOST, "glpat-test-T0KEN-not-real");
    let repo = RepoRef::parse(HOST, "my-group/sub/my-project").unwrap();
    p.comments(&repo, 10).await.unwrap()
}

fn note(list: &CommentList, id: u64) -> &Comment {
    let want = CommentId(format!("note:{id}"));
    list.comments
        .iter()
        .find(|c| c.id == want)
        .unwrap_or_else(|| panic!("note {id} missing"))
}

#[tokio::test]
async fn notes_keep_gitlab_order_and_group_by_discussion() {
    let c = read().await;
    let ids: Vec<&str> = c.comments.iter().map(|c| c.id.0.as_str()).collect();
    assert_eq!(
        ids,
        [
            "note:1126",
            "note:1127",
            "note:1128",
            "note:1129",
            "note:1130",
            "note:1131",
            "note:1132",
            "note:1133",
            "note:1135"
        ]
    );
    let thread = Some(ThreadId("6a9c1750b37d513a43987b574953fceb50b03ce7".into()));
    assert_eq!(note(&c, 1126).thread, thread);
    assert_eq!(note(&c, 1127).thread, thread);
    assert_eq!(note(&c, 1127).parent_id, None);
    assert_eq!(note(&c, 1127).author.login.as_deref(), Some("root"));
    assert!(note(&c, 1126).can_resolve && !note(&c, 1126).resolved);
    assert!(note(&c, 1128).resolved);
}

#[tokio::test]
async fn added_removed_context_positions_map_to_sides() {
    let c = read().await;
    let added = note(&c, 1126).anchor.clone().unwrap();
    assert_eq!(
        (added.path.as_str(), added.from, added.to),
        ("src/widget.rs", None, Some(12))
    );
    let removed = note(&c, 1128).anchor.clone().unwrap();
    assert_eq!((removed.from, removed.to), (Some(7), None));
    // a context line (both lines) sits on the new side
    let context = note(&c, 1129).anchor.clone().unwrap();
    assert_eq!((context.from, context.to), (None, Some(22)));
    assert_eq!(added.commit.as_deref(), Some(HEAD));
}

#[tokio::test]
async fn line_range_maps_to_start() {
    let c = read().await;
    let a = note(&c, 1129).anchor.clone().unwrap();
    assert_eq!(a.start, Some((Side::New, 20)));
    assert_eq!(note(&c, 1126).anchor.as_ref().unwrap().start, None);
}

#[tokio::test]
async fn rename_keeps_old_path() {
    let c = read().await;
    let a = note(&c, 1130).anchor.clone().unwrap();
    assert_eq!(a.path, "src/new_name.rs");
    assert_eq!(a.old_path.as_deref(), Some("src/old_name.rs"));
    assert_eq!(a.to, Some(3));
    assert_eq!(note(&c, 1126).anchor.as_ref().unwrap().old_path, None);
}

#[tokio::test]
async fn file_position_is_file_level() {
    let c = read().await;
    let a = note(&c, 1131).anchor.clone().unwrap();
    assert_eq!(
        (a.path.as_str(), a.from, a.to, a.start),
        ("README.md", None, None, None)
    );
}

#[tokio::test]
async fn note_on_older_version_is_outdated() {
    let c = read().await;
    let old = note(&c, 1132).anchor.clone().unwrap();
    assert!(old.outdated);
    assert_eq!(old.to, Some(40));
    assert_eq!(
        old.commit.as_deref(),
        Some("0b4bc9a49b562e85de7cc9e834518ea6828729b9")
    );
    for id in [1126, 1128, 1129, 1130, 1131] {
        assert!(!note(&c, id).anchor.as_ref().unwrap().outdated, "{id}");
    }
}

#[tokio::test]
async fn system_notes_skipped() {
    let c = read().await;
    assert!(c.comments.iter().all(|n| n.id.0 != "note:1134"));
    assert!(c.comments.iter().all(|n| n.body != "added 1 commit"));
}

#[tokio::test]
async fn non_diff_discussion_is_pr_level_and_replyable() {
    let c = read().await;
    let general = note(&c, 1133);
    assert_eq!(general.anchor, None);
    assert_eq!(
        general.thread,
        Some(ThreadId("e5f6071829304152637a8b9c0d1e2f3456789012".into()))
    );
    assert!(!general.can_resolve);
    assert!(!general.author_is_bot);
}

#[tokio::test]
async fn bot_author_detected_and_bad_note_skipped() {
    let c = read().await;
    assert!(note(&c, 1135).author_is_bot);
    assert!(note(&c, 1135).can_resolve);
    assert_eq!(
        c.skipped,
        [SkippedComment {
            id: None,
            field: "id".into()
        }]
    );
    assert_eq!(c.tasks, None);
}

/// Live GitLab 19.5.1: after a push a note's line moves (new 8 -> 12) but its `line_range`
/// stays 6..8. A range that does not end on the note's own line is not shown.
#[tokio::test]
async fn stale_line_range_after_a_push_is_dropped() {
    let stale: serde_json::Value =
        serde_json::from_str(&fixture("discussion_live_stale_range")).unwrap();
    let mut fresh = stale.clone();
    fresh["id"] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
    fresh["notes"][0]["id"] = 9001.into();
    fresh["notes"][0]["position"]["line_range"]["end"]["new_line"] = 12.into();
    fresh["notes"][0]["position"]["line_range"]["start"]["new_line"] = 10.into();
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(MR))
        .respond_with(ok("mr_detail"))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR}/versions")))
        .respond_with(ok("mr_versions"))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR}/discussions")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([stale, fresh])))
        .mount(&s)
        .await;
    let p = GitLabProvider::new(&s.uri(), HOST, "glpat-test-T0KEN-not-real");
    let repo = RepoRef::parse(HOST, "my-group/sub/my-project").unwrap();
    let c = p.comments(&repo, 10).await.unwrap();
    // GitLab's "changed this line" system replies are skipped
    assert_eq!(c.comments.len(), 2);
    let a = note(&c, 754).anchor.clone().unwrap();
    assert_eq!((a.to, a.start), (Some(12), None));
    let b = note(&c, 9001).anchor.clone().unwrap();
    assert_eq!((b.to, b.start), (Some(12), Some((Side::New, 10))));
}
