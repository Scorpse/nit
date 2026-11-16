use super::super::composer::MemoryPoster;
use super::*;
use chrono::TimeZone;
use prcore::model::PrState;
use std::sync::Arc;
use std::time::{Duration, Instant};

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn user(n: &str) -> UserRef {
    UserRef {
        id: format!("{{{n}}}"),
        display_name: n.into(),
        login: None,
    }
}

fn repo() -> RepoRef {
    RepoRef::parse_bitbucket("acme/api").unwrap()
}

fn pr(id: u64, reviewers: Vec<(UserRef, ReviewState)>) -> PullRequest {
    let t = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    PullRequest {
        repo: repo(),
        id,
        title: format!("pr {id}"),
        state: PrState::Open,
        draft: false,
        author: user("author"),
        source_branch: format!("b{id}"),
        dest_branch: "main".into(),
        source_commit: HEAD.into(),
        dest_commit: "d".into(),
        source_repo: repo(),
        comment_count: 0,
        task_count: 0,
        reviewers: reviewers
            .into_iter()
            .map(|(user, state)| Reviewer { user, state })
            .collect(),
        created_on: t,
        updated_on: t,
        forge: Default::default(),
        account: Default::default(),
        web_url: String::new(),
        node_id: None,
        review_requested_from_me: false,
        locked: false,
        checks: Default::default(),
        has_conflicts: false,
        new_commits: false,
    }
}

fn ctx_with_stack() -> ReviewContext {
    let parent = pr(1, vec![(user("bo"), ReviewState::Pending)]);
    ReviewContext {
        me: Some(user("me")),
        stack: Some(Stack {
            repo: repo(),
            members: vec![1, 2],
            edges: vec![(1, 2)],
        }),
        prs: [(1, parent), (2, pr(2, vec![]))].into_iter().collect(),
    }
}

fn actions(poster: &Arc<MemoryPoster>) -> ReviewActions {
    let mut a = ReviewActions::new(ctx_with_stack());
    a.connect(poster.clone(), None);
    a
}

fn mine(p: &PullRequest) -> Option<ReviewState> {
    my_state(p, Some(&user("me")))
}

fn wait(a: &mut ReviewActions, p: &mut PullRequest) -> bool {
    let start = Instant::now();
    let mut stale = false;
    while a.running() {
        stale |= a.poll(p, HEAD);
        assert!(start.elapsed() < Duration::from_secs(10), "action hung");
        std::thread::sleep(Duration::from_millis(5));
    }
    stale
}

#[test]
fn action_toggle_state_from_my_review_state() {
    let me = user("me");
    let other = user("bo");
    let p = |s: ReviewState| {
        pr(
            2,
            vec![(other.clone(), ReviewState::Approved), (me.clone(), s)],
        )
    };
    let b = buttons(my_state(&p(ReviewState::Approved), Some(&me)));
    assert_eq!(
        (label(b.approve), label(b.request)),
        ("Remove approval", "Request changes")
    );
    let b = buttons(my_state(&p(ReviewState::ChangesRequested), Some(&me)));
    assert_eq!(
        (label(b.approve), label(b.request)),
        ("Approve", "Withdraw request")
    );
    assert_eq!(b.request, ReviewAction::ClearRequestChanges);
    let b = buttons(my_state(&p(ReviewState::Pending), Some(&me)));
    assert_eq!(
        (b.approve, b.request),
        (ReviewAction::Approve, ReviewAction::RequestChanges)
    );
    assert_eq!(b.note, None);
    // someone else's approval is not mine; not listed means not reviewed yet
    let only_other = pr(2, vec![(other.clone(), ReviewState::Approved)]);
    assert_eq!(my_state(&only_other, Some(&me)), Some(ReviewState::Pending));
    // identity unknown: both plain buttons and a note
    let b = buttons(my_state(&p(ReviewState::Approved), None));
    assert_eq!(
        (b.approve, b.request),
        (ReviewAction::Approve, ReviewAction::RequestChanges)
    );
    assert_eq!(b.note, Some(UNKNOWN_ME));
    assert_eq!(
        question(ReviewAction::Approve),
        "Approve this pull request?"
    );
    assert_eq!(
        question(ReviewAction::RequestChanges),
        "Request changes on this pull request?"
    );
    assert_eq!(
        question(ReviewAction::ClearApproval),
        "Remove your approval?"
    );
}

/// GitHub requires a body for REQUEST_CHANGES: nothing is sent without one there, and
/// the text is sent with the action. Other forges ask for none.
#[test]
fn request_changes_needs_body_on_github_only() {
    use prcore::model::Forge;
    assert!(body_required(Forge::GitHub, ReviewAction::RequestChanges));
    assert!(!body_required(Forge::GitHub, ReviewAction::Approve));
    assert!(!body_required(
        Forge::BitbucketCloud,
        ReviewAction::RequestChanges
    ));
    assert!(!body_required(Forge::GitLab, ReviewAction::RequestChanges));
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    p.forge = Forge::GitHub;
    a.request(ReviewAction::RequestChanges, &p, false, 0, 0);
    a.confirm.as_mut().unwrap().body = "  \n".into();
    assert!(!a.confirm(&repo(), &mut p, HEAD, false), "nothing sent");
    assert!(a.confirm.is_some(), "the dialog stays open");
    assert!(a.error.is_some());
    assert!(poster.reviews.lock().unwrap().is_empty());
    a.confirm.as_mut().unwrap().body = "Please split this.".into();
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(
        *poster.review_bodies.lock().unwrap(),
        [Some("Please split this.".to_string())]
    );
    // Bitbucket: no body asked for, none sent
    let mut b = pr(2, vec![]);
    a.request(ReviewAction::RequestChanges, &b, false, 0, 0);
    assert!(a.confirm(&repo(), &mut b, HEAD, false));
    wait(&mut a, &mut b);
    assert_eq!(poster.review_bodies.lock().unwrap()[1], None);
}

#[test]
fn warning_shown_when_parent_unapproved() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    let c = a.confirm.clone().unwrap();
    assert_eq!(
        c.warnings,
        ["Parent PR #1 is not approved or merged yet; it should land first."]
    );
    // unresolved threads and unsent drafts are warned about too
    a.request(ReviewAction::Approve, &p, false, 2, 1);
    let w = a.confirm.clone().unwrap().warnings;
    assert_eq!(w.len(), 3, "{w:?}");
    assert_eq!(w[1], "2 unresolved comment threads.");
    assert_eq!(w[2], "1 unsent comment draft: submit them first?");
    // other actions carry no approval warnings
    a.request(ReviewAction::RequestChanges, &p, false, 2, 1);
    assert!(a.confirm.as_ref().unwrap().warnings.is_empty());
    // a warning never blocks
    a.request(ReviewAction::Approve, &p, false, 2, 1);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(*poster.reviews.lock().unwrap(), [ReviewAction::Approve]);
    assert_eq!(mine(&p), Some(ReviewState::Approved));
    assert_eq!(a.done.as_deref(), Some("Approved."));
}

#[test]
fn actions_disabled_when_stale() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    assert_eq!(a.disabled_reason(true), Some(STALE_ACTIONS));
    assert_eq!(
        STALE_ACTIONS,
        "The PR changed since you opened it. Reload before approving."
    );
    a.request(ReviewAction::Approve, &p, true, 0, 0);
    assert!(a.confirm.is_none(), "no confirmation while stale");
    // stale arrived while the confirmation was open: nothing is sent
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(!a.confirm(&repo(), &mut p, HEAD, true));
    assert!(poster.reviews.lock().unwrap().is_empty());
    assert_eq!(mine(&p), Some(ReviewState::Pending));
}

#[test]
fn second_click_while_in_flight_does_nothing() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    *poster.hold_review.lock().unwrap() = true;
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    assert!(a.running());
    assert!(a.disabled_reason(false).is_some(), "buttons disabled");
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm.is_none());
    assert!(!a.confirm(&repo(), &mut p, HEAD, false));
    poster.release();
    wait(&mut a, &mut p);
    assert_eq!(poster.reviews.lock().unwrap().len(), 1, "posted once");
}

#[test]
fn action_error_reverts_optimistic_state() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    poster
        .review_script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Rejected("access denied".into())));
    *poster.hold_review.lock().unwrap() = true;
    let mut a = actions(&poster);
    let mut p = pr(2, vec![(user("me"), ReviewState::ChangesRequested)]);
    a.request(ReviewAction::ClearRequestChanges, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    assert_eq!(mine(&p), Some(ReviewState::Pending), "shown at once");
    poster.release();
    wait(&mut a, &mut p);
    assert_eq!(mine(&p), Some(ReviewState::ChangesRequested), "put back");
    assert_eq!(
        a.error.as_deref(),
        Some("Could not withdraw the request for changes: rejected by the server (access denied)")
    );
    assert!(a.done.is_none());
    // a participant added by the optimistic update is removed again
    poster
        .review_script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Network("network error".into())));
    let mut q = pr(2, vec![]);
    a.request(ReviewAction::Approve, &q, false, 0, 0);
    assert!(a.confirm(&repo(), &mut q, HEAD, false));
    wait(&mut a, &mut q);
    assert!(q.reviewers.is_empty());
}

/// A lost answer (timeout, reset, 5xx after sending) is not a failure: the PR is read
/// again and its reviewers adopted; it shows as done when the server has it.
#[test]
fn unknown_outcome_reads_the_pr_again_instead_of_failing() {
    let unconfirmed = |poster: &MemoryPoster| {
        poster
            .review_script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Unconfirmed));
    };
    // applied on the server
    let poster = Arc::new(MemoryPoster::new(HEAD));
    *poster.pr.lock().unwrap() = Some(pr(2, vec![]));
    *poster.me.lock().unwrap() = Some(user("me"));
    unconfirmed(&poster);
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(mine(&p), Some(ReviewState::Approved));
    assert_eq!(a.done.as_deref(), Some("Approved."));
    assert!(a.error.is_none() && a.unconfirmed.is_none());

    // not applied: the server's reviewers are shown, unconfirmed rather than failed
    let poster = Arc::new(MemoryPoster::new(HEAD));
    *poster.pr.lock().unwrap() = Some(pr(2, vec![(user("bo"), ReviewState::Approved)]));
    unconfirmed(&poster);
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(
        mine(&p),
        Some(ReviewState::Pending),
        "the refreshed list (I am not on it)"
    );
    assert_eq!(p.reviewers.len(), 1);
    assert!(a.error.is_none() && a.done.is_none());
    let n = a.unconfirmed.clone().unwrap_or_default();
    assert!(
        n.contains("Not confirmed") && n.contains("refreshed"),
        "{n}"
    );

    // the PR cannot be read either: the earlier state stays, still not a failure
    let poster = Arc::new(MemoryPoster::new(HEAD));
    unconfirmed(&poster);
    let mut a = actions(&poster);
    let mut p = pr(2, vec![(user("me"), ReviewState::ChangesRequested)]);
    a.request(ReviewAction::ClearRequestChanges, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(mine(&p), Some(ReviewState::ChangesRequested));
    assert!(a.error.is_none() && a.done.is_none());
    let n = a.unconfirmed.clone().unwrap_or_default();
    assert!(n.contains("Not confirmed") && n.contains("Reload"), "{n}");
}

/// The inbox keeps its own copy of the PR: it is handed the reviewers after an action
/// that went through (or was found applied), never after a failure.
#[test]
fn reviewers_are_handed_on_after_success_only() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    *poster.pr.lock().unwrap() = Some(pr(2, vec![(user("bo"), ReviewState::Approved)]));
    *poster.me.lock().unwrap() = Some(user("me"));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    assert_eq!(a.take_reviewers_update(), None, "not while it runs");
    wait(&mut a, &mut p);
    assert_eq!(a.take_reviewers_update(), Some(p.reviewers.clone()));
    assert_eq!(p.reviewers.len(), 2, "the server's list");
    assert_eq!(a.take_reviewers_update(), None, "handed on once");
    poster
        .review_script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Rejected("access denied".into())));
    a.request(ReviewAction::RequestChanges, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(a.take_reviewers_update(), None);
}

#[test]
fn confirm_required_before_any_action() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    assert!(
        !a.confirm(&repo(), &mut p, HEAD, false),
        "no confirmation open"
    );
    for act in [
        ReviewAction::Approve,
        ReviewAction::RequestChanges,
        ReviewAction::ClearApproval,
        ReviewAction::ClearRequestChanges,
    ] {
        a.request(act, &p, false, 0, 0);
        assert_eq!(a.confirm.as_ref().map(|c| c.action), Some(act));
        a.cancel();
    }
    std::thread::sleep(Duration::from_millis(20));
    assert!(poster.reviews.lock().unwrap().is_empty());
    assert!(!a.running());
    a.request(ReviewAction::RequestChanges, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(
        *poster.reviews.lock().unwrap(),
        [ReviewAction::RequestChanges]
    );
    assert_eq!(mine(&p), Some(ReviewState::ChangesRequested));
}

#[test]
fn approving_while_changes_requested_notes_the_other_state_and_sends_one_call() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![(user("me"), ReviewState::ChangesRequested)]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm.as_ref().unwrap().note.is_some());
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(*poster.reviews.lock().unwrap(), [ReviewAction::Approve]);
}

#[test]
fn server_head_moved_after_action_makes_the_screen_stale() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    *poster.pr.lock().unwrap() = Some(pr(2, vec![]));
    *poster.head_after_review.lock().unwrap() = Some("fedcba9876543210".into());
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    assert!(wait(&mut a, &mut p));
    assert_eq!(
        poster.reviews.lock().unwrap().len(),
        1,
        "sent before the push"
    );
    // the abbreviated form of the same head is not a change
    let poster = Arc::new(MemoryPoster::new(&HEAD[..12]));
    let mut a = actions(&poster);
    a.request(ReviewAction::ClearApproval, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    assert!(!wait(&mut a, &mut p));
}

#[test]
fn reviewers_are_read_back_from_the_server_after_an_action() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    // someone else approved meanwhile; the server is the truth after my action
    *poster.pr.lock().unwrap() = Some(pr(2, vec![(user("bo"), ReviewState::Approved)]));
    *poster.me.lock().unwrap() = Some(user("me"));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    assert!(!wait(&mut a, &mut p));
    let got: Vec<(String, ReviewState)> = p
        .reviewers
        .iter()
        .map(|r| (r.user.display_name.clone(), r.state.clone()))
        .collect();
    assert_eq!(
        got,
        [
            ("bo".to_string(), ReviewState::Approved),
            ("me".to_string(), ReviewState::Approved)
        ]
    );
    assert_eq!(
        label(a.buttons(&p).approve),
        "Remove approval",
        "the button follows"
    );
    // a PR that cannot be read back keeps the optimistic state
    *poster.pr.lock().unwrap() = None;
    a.request(ReviewAction::RequestChanges, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    wait(&mut a, &mut p);
    assert_eq!(mine(&p), Some(ReviewState::ChangesRequested));
    // an unloaded screen never sends
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(!a.confirm(&repo(), &mut p, "", false));
    assert_eq!(poster.reviews.lock().unwrap().len(), 2);
}

#[test]
fn approve_blocked_when_head_moved_sends_nothing() {
    // the PR was pushed to after the screen fetched it
    let poster = Arc::new(MemoryPoster::new("fedcba9876543210"));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    assert!(wait(&mut a, &mut p), "the screen is stale now");
    assert!(poster.reviews.lock().unwrap().is_empty(), "nothing sent");
    assert_eq!(mine(&p), Some(ReviewState::Pending), "put back");
    assert_eq!(a.error.as_deref(), Some(STALE_ACTIONS));
    assert!(a.done.is_none());
}

#[test]
fn head_probe_error_blocks_action() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    poster
        .head_script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Network("network error".into())));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.request(ReviewAction::RequestChanges, &p, false, 0, 0);
    assert!(a.confirm(&repo(), &mut p, HEAD, false));
    assert!(!wait(&mut a, &mut p), "not known to be stale");
    assert!(poster.reviews.lock().unwrap().is_empty(), "nothing sent");
    assert_eq!(mine(&p), Some(ReviewState::Pending));
    let e = a.error.clone().unwrap_or_default();
    assert!(
        e.starts_with("Could not confirm the PR is unchanged"),
        "{e}"
    );
    assert!(a.done.is_none());
}

/// GitLab writes now: its review screen is not read-only, offers Approve, Request
/// changes and Unapprove (not a clear of requested changes), and sends them.
#[test]
fn gitlab_review_actions_come_from_its_capabilities() {
    use prcore::model::Forge;
    let caps = providers::provider::capabilities_for(Forge::GitLab);
    assert!(!caps.read_only);
    assert!(caps.allows(ReviewAction::Approve) && caps.allows(ReviewAction::RequestChanges));
    assert!(caps.allows(ReviewAction::ClearApproval));
    assert!(!caps.allows(ReviewAction::ClearRequestChanges));
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut a = actions(&poster);
    a.read_only = caps.read_only;
    let mut p = pr(2, vec![]);
    p.forge = Forge::GitLab;
    assert_eq!(a.disabled_reason(false), None);
    for act in [ReviewAction::Approve, ReviewAction::RequestChanges] {
        a.request(act, &p, false, 0, 0);
        assert!(a.confirm(&repo(), &mut p, HEAD, false), "{act:?}");
        wait(&mut a, &mut p);
    }
    assert_eq!(poster.reviews.lock().unwrap().len(), 2);
    // GitLab takes no text with a change request
    assert_eq!(poster.review_bodies.lock().unwrap()[1], None);
}

#[test]
fn a_read_only_forge_refuses_every_action_before_anything_is_sent() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut a = actions(&poster);
    let mut p = pr(2, vec![]);
    a.read_only = true;
    assert_eq!(
        a.disabled_reason(false),
        Some(crate::review::composer::READ_ONLY_TIP)
    );
    a.request(ReviewAction::Approve, &p, false, 0, 0);
    assert!(a.confirm.is_none(), "no confirmation opens");
    assert!(!a.confirm(&repo(), &mut p, HEAD, false));
    assert!(poster.reviews.lock().unwrap().is_empty(), "nothing sent");
}

#[test]
fn github_page_opens_only_on_the_repository_host() {
    let gh = RepoRef::parse("github.com", "octo-org/api").unwrap();
    let mut pr = crate::demo::demo_data().1.remove(0);
    pr.forge = prcore::model::Forge::GitHub;
    pr.web_url = "https://github.com/octo-org/api/pull/7".into();
    assert_eq!(open_web_label(&pr), "Open in GitHub");
    assert_eq!(
        forge_page_url(&gh, &pr).as_deref(),
        Some("https://github.com/octo-org/api/pull/7")
    );
    for bad in [
        "https://evil.example/octo-org/api/pull/7",
        "http://github.com/octo-org/api/pull/7",
        "https://github.com.evil.example/x",
        "https://github.com/a&calc",
        "https://github.com/a b",
        "",
    ] {
        pr.web_url = bad.into();
        assert_eq!(forge_page_url(&gh, &pr), None, "{bad}");
    }
}

#[test]
fn an_http_page_opens_only_on_a_private_repository_host() {
    let gl = RepoRef::parse("192.168.1.20:8080", "root/nit-live-test").unwrap();
    let mut pr = crate::demo::demo_data().1.remove(0);
    pr.forge = prcore::model::Forge::GitLab;
    let page = "http://192.168.1.20:8080/root/nit-live-test/-/merge_requests/1";
    pr.web_url = page.into();
    assert_eq!(forge_page_url(&gl, &pr).as_deref(), Some(page));
    for bad in [
        "http://192.168.1.20:8081/root/nit-live-test/-/merge_requests/1",
        "http://192.168.1.20:8080.evil.example/x",
        "http://192.168.1.88:8080/x",
    ] {
        pr.web_url = bad.into();
        assert_eq!(forge_page_url(&gl, &pr), None, "{bad}");
    }
    let public = RepoRef::parse("gitlab.example", "grp/proj").unwrap();
    pr.web_url = "http://gitlab.example/grp/proj/-/merge_requests/1".into();
    assert_eq!(forge_page_url(&public, &pr), None);
}

#[test]
fn open_in_bitbucket_url_built_only_from_validated_parts() {
    let r = |ws: &str, slug: &str| RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: ws.into(),
        slug: slug.into(),
    };
    assert_eq!(
        bitbucket_url(&r("acme", "my-repo.v2_x"), 42).as_deref(),
        Some("https://bitbucket.org/acme/my-repo.v2_x/pull-requests/42")
    );
    for (ws, slug) in [
        ("..", "api"),
        ("acme", ".."),
        ("acme", "."),
        ("acme", "my repo"),
        ("acme", "a\"b"),
        ("acme", "a'b"),
        ("acme", "a&calc"),
        ("ac&me", "api"),
        ("acme", "a/b"),
        ("acme", "a\\b"),
        ("acme", ""),
        ("", "api"),
        ("acme", "a|b"),
        ("acme", "a%20b"),
        ("acme", "ap\u{e9}"),
    ] {
        assert_eq!(bitbucket_url(&r(ws, slug), 1), None, "{ws:?}/{slug:?}");
    }
    assert_eq!(bitbucket_url(&r("acme", "api"), 0), None);
    assert!(open_in_bitbucket(&r("acme", "a&calc"), 1).is_err());
}
